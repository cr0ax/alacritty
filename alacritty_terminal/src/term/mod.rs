//! Exports the `Term` type which is a high-level API for the Grid.

use std::cmp::{max, min};
use std::ops::{Index, IndexMut, Range};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{io, mem, ptr, str};

use log::{debug, trace};
use serde::{Deserialize, Serialize};
use unicode_width::UnicodeWidthChar;

use crate::ansi::{
    self, Attr, CharsetIndex, Color, CursorStyle, Handler, NamedColor, StandardCharset, TermInfo,
};
use crate::config::{Config, VisualBellAnimation};
use crate::event::{Event, EventListener};
use crate::grid::{
    BidirectionalIterator, DisplayIter, Grid, GridCell, IndexRegion, Indexed, Scroll,
};
use crate::index::{self, Column, IndexRange, Line, Point, Side};
use crate::selection::{Selection, SelectionRange};
use crate::term::cell::{Cell, Flags, LineLength};
use crate::term::color::{Rgb, DIM_FACTOR};
use crate::vi_mode::{ViModeCursor, ViMotion};

pub mod cell;
pub mod color;

/// Used to match equal brackets, when performing a bracket-pair selection.
const BRACKET_PAIRS: [(char, char); 4] = [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>')];

/// Max size of the window title stack.
const TITLE_STACK_MAX_DEPTH: usize = 4096;

/// Default tab interval, corresponding to terminfo `it` value.
const INITIAL_TABSTOPS: usize = 8;

/// Minimum number of columns and lines.
const MIN_SIZE: usize = 2;

/// A type that can expand a given point to a region.
///
/// Usually this is implemented for some 2-D array type since
/// points are two dimensional indices.
pub trait Search {
    /// Find the nearest semantic boundary _to the left_ of provided point.
    fn semantic_search_left(&self, _: Point<usize>) -> Point<usize>;
    /// Find the nearest semantic boundary _to the point_ of provided point.
    fn semantic_search_right(&self, _: Point<usize>) -> Point<usize>;
    /// Find the beginning of a line, following line wraps.
    fn line_search_left(&self, _: Point<usize>) -> Point<usize>;
    /// Find the end of a line, following line wraps.
    fn line_search_right(&self, _: Point<usize>) -> Point<usize>;
    /// Find the nearest matching bracket.
    fn bracket_search(&self, _: Point<usize>) -> Option<Point<usize>>;
}

impl<T> Search for Term<T> {
    fn semantic_search_left(&self, mut point: Point<usize>) -> Point<usize> {
        // Limit the starting point to the last line in the history.
        point.line = min(point.line, self.grid.len() - 1);

        let mut iter = self.grid.iter_from(point);
        let last_col = self.grid.num_cols() - Column(1);

        while let Some(cell) = iter.prev() {
            if !cell.flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER)
                && self.semantic_escape_chars.contains(cell.c)
            {
                break;
            }

            if iter.point().col == last_col && !cell.flags.contains(Flags::WRAPLINE) {
                // Cut off if on new line or hit escape char.
                break;
            }

            point = iter.point();
        }

        point
    }

    fn semantic_search_right(&self, mut point: Point<usize>) -> Point<usize> {
        // Limit the starting point to the last line in the history.
        point.line = min(point.line, self.grid.len() - 1);

        let mut iter = self.grid.iter_from(point);
        let last_col = self.grid.num_cols() - 1;

        while let Some(cell) = iter.next() {
            if !cell.flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER)
                && self.semantic_escape_chars.contains(cell.c)
            {
                break;
            }

            point = iter.point();

            if point.col == last_col && !cell.flags.contains(Flags::WRAPLINE) {
                // Cut off if on new line or hit escape char.
                break;
            }
        }

        point
    }

    fn line_search_left(&self, mut point: Point<usize>) -> Point<usize> {
        while point.line + 1 < self.grid.len()
            && self.grid[point.line + 1][self.grid.num_cols() - 1].flags.contains(Flags::WRAPLINE)
        {
            point.line += 1;
        }

        point.col = Column(0);

        point
    }

    fn line_search_right(&self, mut point: Point<usize>) -> Point<usize> {
        while self.grid[point.line][self.grid.num_cols() - 1].flags.contains(Flags::WRAPLINE) {
            point.line -= 1;
        }

        point.col = self.grid.num_cols() - 1;

        point
    }

    fn bracket_search(&self, point: Point<usize>) -> Option<Point<usize>> {
        let start_char = self.grid[point.line][point.col].c;

        // Find the matching bracket we're looking for.
        let (forwards, end_char) = BRACKET_PAIRS.iter().find_map(|(open, close)| {
            if open == &start_char {
                Some((true, *close))
            } else if close == &start_char {
                Some((false, *open))
            } else {
                None
            }
        })?;

        let mut iter = self.grid.iter_from(point);

        // For every character match that equals the starting bracket, we
        // ignore one bracket of the opposite type.
        let mut skip_pairs = 0;

        loop {
            // Check the next cell.
            let cell = if forwards { iter.next() } else { iter.prev() };

            // Break if there are no more cells.
            let c = match cell {
                Some(cell) => cell.c,
                None => break,
            };

            // Check if the bracket matches.
            if c == end_char && skip_pairs == 0 {
                return Some(iter.point());
            } else if c == start_char {
                skip_pairs += 1;
            } else if c == end_char {
                skip_pairs -= 1;
            }
        }

        None
    }
}

/// Cursor storing all information relevant for rendering.
#[derive(Debug, Eq, PartialEq, Copy, Clone, Deserialize)]
struct RenderableCursor {
    text_color: Option<Rgb>,
    cursor_color: Option<Rgb>,
    key: CursorKey,
    point: Point,
    rendered: bool,
}

/// A key for caching cursor glyphs.
#[derive(Debug, Eq, PartialEq, Copy, Clone, Hash, Deserialize)]
pub struct CursorKey {
    pub style: CursorStyle,
    pub is_wide: bool,
}

/// Iterator that yields cells needing render.
///
/// Yields cells that require work to be displayed (that is, not a an empty
/// background cell). Additionally, this manages some state of the grid only
/// relevant for rendering like temporarily changing the cell with the cursor.
///
/// This manages the cursor during a render. The cursor location is inverted to
/// draw it, and reverted after drawing to maintain state.
pub struct RenderableCellsIter<'a, C> {
    inner: DisplayIter<'a, Cell>,
    grid: &'a Grid<Cell>,
    cursor: RenderableCursor,
    config: &'a Config<C>,
    colors: &'a color::List,
    selection: Option<SelectionRange<Line>>,
}

impl<'a, C> RenderableCellsIter<'a, C> {
    /// Create the renderable cells iterator.
    ///
    /// The cursor and terminal mode are required for properly displaying the
    /// cursor.
    fn new<'b, T>(
        term: &'b Term<T>,
        config: &'b Config<C>,
        selection: Option<SelectionRange>,
    ) -> RenderableCellsIter<'b, C> {
        let grid = &term.grid;

        let inner = grid.display_iter();

        let selection_range = selection.and_then(|span| {
            let (limit_start, limit_end) = if span.is_block {
                (span.start.col, span.end.col)
            } else {
                (Column(0), grid.num_cols() - 1)
            };

            // Do not render completely offscreen selection.
            let viewport_start = grid.display_offset();
            let viewport_end = viewport_start + grid.num_lines().0;
            if span.end.line >= viewport_end || span.start.line < viewport_start {
                return None;
            }

            // Get on-screen lines of the selection's locations.
            let mut start = grid.clamp_buffer_to_visible(span.start);
            let mut end = grid.clamp_buffer_to_visible(span.end);

            // Trim start/end with partially visible block selection.
            start.col = max(limit_start, start.col);
            end.col = min(limit_end, end.col);

            Some(SelectionRange::new(start, end, span.is_block))
        });

        RenderableCellsIter {
            cursor: term.renderable_cursor(config),
            grid,
            inner,
            selection: selection_range,
            config,
            colors: &term.colors,
        }
    }

    /// Check selection state of a cell.
    fn is_selected(&self, point: Point) -> bool {
        let selection = match self.selection {
            Some(selection) => selection,
            None => return false,
        };

        // Do not invert block cursor at selection boundaries.
        if self.cursor.key.style == CursorStyle::Block
            && self.cursor.point == point
            && (selection.start == point
                || selection.end == point
                || (selection.is_block
                    && ((selection.start.line == point.line && selection.end.col == point.col)
                        || (selection.end.line == point.line && selection.start.col == point.col))))
        {
            return false;
        }

        // Point itself is selected.
        if selection.contains(point.col, point.line) {
            return true;
        }

        let num_cols = self.grid.num_cols();
        let cell = self.grid[&point];

        // Check if wide char's spacers are selected.
        if cell.flags.contains(Flags::WIDE_CHAR) {
            let prevprev = point.sub(num_cols, 2);
            let prev = point.sub(num_cols, 1);
            let next = point.add(num_cols, 1);

            // Check trailing spacer.
            selection.contains(next.col, next.line)
                // Check line-wrapping, leading spacer.
                || (self.grid[&prev].flags.contains(Flags::WIDE_CHAR_SPACER)
                    && !self.grid[&prevprev].flags.contains(Flags::WIDE_CHAR)
                    && selection.contains(prev.col, prev.line))
        } else if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            // Check if spacer's wide char is selected.
            let prev = point.sub(num_cols, 1);

            if self.grid[&prev].flags.contains(Flags::WIDE_CHAR) {
                // Check previous cell for trailing spacer.
                self.is_selected(prev)
            } else {
                // Check next cell for line-wrapping, leading spacer.
                self.is_selected(point.add(num_cols, 1))
            }
        } else {
            false
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub enum RenderableCellContent {
    Chars([char; cell::MAX_ZEROWIDTH_CHARS + 1]),
    Cursor(CursorKey),
}

#[derive(Copy, Clone, Debug)]
pub struct RenderableCell {
    /// A _Display_ line (not necessarily an _Active_ line).
    pub line: Line,
    pub column: Column,
    pub inner: RenderableCellContent,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bg_alpha: f32,
    pub flags: Flags,
}

impl RenderableCell {
    fn new<C>(
        config: &Config<C>,
        colors: &color::List,
        cell: Indexed<Cell>,
        selected: bool,
    ) -> Self {
        // Lookup RGB values.
        let mut fg_rgb = Self::compute_fg_rgb(config, colors, cell.fg, cell.flags);
        let mut bg_rgb = Self::compute_bg_rgb(colors, cell.bg);
        let mut bg_alpha = Self::compute_bg_alpha(cell.bg);

        let selection_background = config.colors.selection.background;
        if let (true, Some(col)) = (selected, selection_background) {
            // Override selection background with config colors.
            bg_rgb = col;
            bg_alpha = 1.0;
        } else if selected ^ cell.inverse() {
            if fg_rgb == bg_rgb && !cell.flags.contains(Flags::HIDDEN) {
                // Reveal inversed text when fg/bg is the same.
                fg_rgb = colors[NamedColor::Background];
                bg_rgb = colors[NamedColor::Foreground];
            } else {
                // Invert cell fg and bg colors.
                mem::swap(&mut fg_rgb, &mut bg_rgb);
            }

            bg_alpha = 1.0;
        }

        // Override selection text with config colors.
        if let (true, Some(col)) = (selected, config.colors.selection.text) {
            fg_rgb = col;
        }

        RenderableCell {
            line: cell.line,
            column: cell.column,
            inner: RenderableCellContent::Chars(cell.chars()),
            fg: fg_rgb,
            bg: bg_rgb,
            bg_alpha,
            flags: cell.flags,
        }
    }

    fn compute_fg_rgb<C>(config: &Config<C>, colors: &color::List, fg: Color, flags: Flags) -> Rgb {
        match fg {
            Color::Spec(rgb) => match flags & Flags::DIM {
                Flags::DIM => rgb * DIM_FACTOR,
                _ => rgb,
            },
            Color::Named(ansi) => {
                match (config.draw_bold_text_with_bright_colors(), flags & Flags::DIM_BOLD) {
                    // If no bright foreground is set, treat it like the BOLD flag doesn't exist.
                    (_, Flags::DIM_BOLD)
                        if ansi == NamedColor::Foreground
                            && config.colors.primary.bright_foreground.is_none() =>
                    {
                        colors[NamedColor::DimForeground]
                    },
                    // Draw bold text in bright colors *and* contains bold flag.
                    (true, Flags::BOLD) => colors[ansi.to_bright()],
                    // Cell is marked as dim and not bold.
                    (_, Flags::DIM) | (false, Flags::DIM_BOLD) => colors[ansi.to_dim()],
                    // None of the above, keep original color..
                    _ => colors[ansi],
                }
            },
            Color::Indexed(idx) => {
                let idx = match (
                    config.draw_bold_text_with_bright_colors(),
                    flags & Flags::DIM_BOLD,
                    idx,
                ) {
                    (true, Flags::BOLD, 0..=7) => idx as usize + 8,
                    (false, Flags::DIM, 8..=15) => idx as usize - 8,
                    (false, Flags::DIM, 0..=7) => idx as usize + 260,
                    _ => idx as usize,
                };

                colors[idx]
            },
        }
    }

    #[inline]
    fn compute_bg_alpha(bg: Color) -> f32 {
        if bg == Color::Named(NamedColor::Background) {
            0.
        } else {
            1.
        }
    }

    #[inline]
    fn compute_bg_rgb(colors: &color::List, bg: Color) -> Rgb {
        match bg {
            Color::Spec(rgb) => rgb,
            Color::Named(ansi) => colors[ansi],
            Color::Indexed(idx) => colors[idx],
        }
    }
}

impl<'a, C> Iterator for RenderableCellsIter<'a, C> {
    type Item = RenderableCell;

    /// Gets the next renderable cell.
    ///
    /// Skips empty (background) cells and applies any flags to the cell state
    /// (eg. invert fg and bg colors).
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.cursor.point.line == self.inner.line()
                && self.cursor.point.col == self.inner.column()
            {
                let selected = self.is_selected(self.cursor.point);

                // Handle cell below cursor.
                if self.cursor.rendered {
                    let mut cell =
                        RenderableCell::new(self.config, self.colors, self.inner.next()?, selected);

                    if self.cursor.key.style == CursorStyle::Block {
                        mem::swap(&mut cell.bg, &mut cell.fg);

                        if let Some(color) = self.cursor.text_color {
                            cell.fg = color;
                        }
                    }

                    return Some(cell);
                } else {
                    // Handle cursor.
                    self.cursor.rendered = true;

                    let buffer_point = self.grid.visible_to_buffer(self.cursor.point);
                    let cell = Indexed {
                        inner: self.grid[buffer_point.line][buffer_point.col],
                        column: self.cursor.point.col,
                        line: self.cursor.point.line,
                    };

                    let mut renderable_cell =
                        RenderableCell::new(self.config, self.colors, cell, selected);

                    renderable_cell.inner = RenderableCellContent::Cursor(self.cursor.key);

                    if let Some(color) = self.cursor.cursor_color {
                        renderable_cell.fg = color;
                    }

                    return Some(renderable_cell);
                }
            } else {
                let cell = self.inner.next()?;

                let selected = self.is_selected(Point::new(cell.line, cell.column));

                if !cell.is_empty() || selected {
                    return Some(RenderableCell::new(self.config, self.colors, cell, selected));
                }
            }
        }
    }
}

pub mod mode {
    use bitflags::bitflags;

    bitflags! {
        pub struct TermMode: u32 {
            const NONE                = 0;
            const SHOW_CURSOR         = 0b0000_0000_0000_0000_0001;
            const APP_CURSOR          = 0b0000_0000_0000_0000_0010;
            const APP_KEYPAD          = 0b0000_0000_0000_0000_0100;
            const MOUSE_REPORT_CLICK  = 0b0000_0000_0000_0000_1000;
            const BRACKETED_PASTE     = 0b0000_0000_0000_0001_0000;
            const SGR_MOUSE           = 0b0000_0000_0000_0010_0000;
            const MOUSE_MOTION        = 0b0000_0000_0000_0100_0000;
            const LINE_WRAP           = 0b0000_0000_0000_1000_0000;
            const LINE_FEED_NEW_LINE  = 0b0000_0000_0001_0000_0000;
            const ORIGIN              = 0b0000_0000_0010_0000_0000;
            const INSERT              = 0b0000_0000_0100_0000_0000;
            const FOCUS_IN_OUT        = 0b0000_0000_1000_0000_0000;
            const ALT_SCREEN          = 0b0000_0001_0000_0000_0000;
            const MOUSE_DRAG          = 0b0000_0010_0000_0000_0000;
            const MOUSE_MODE          = 0b0000_0010_0000_0100_1000;
            const UTF8_MOUSE          = 0b0000_0100_0000_0000_0000;
            const ALTERNATE_SCROLL    = 0b0000_1000_0000_0000_0000;
            const VI                  = 0b0001_0000_0000_0000_0000;
            const ANY                 = std::u32::MAX;
        }
    }

    impl Default for TermMode {
        fn default() -> TermMode {
            TermMode::SHOW_CURSOR | TermMode::LINE_WRAP | TermMode::ALTERNATE_SCROLL
        }
    }
}

pub use crate::term::mode::TermMode;

pub struct VisualBell {
    /// Visual bell animation.
    animation: VisualBellAnimation,

    /// Visual bell duration.
    duration: Duration,

    /// The last time the visual bell rang, if at all.
    start_time: Option<Instant>,
}

fn cubic_bezier(p0: f64, p1: f64, p2: f64, p3: f64, x: f64) -> f64 {
    (1.0 - x).powi(3) * p0
        + 3.0 * (1.0 - x).powi(2) * x * p1
        + 3.0 * (1.0 - x) * x.powi(2) * p2
        + x.powi(3) * p3
}

impl VisualBell {
    pub fn new<C>(config: &Config<C>) -> VisualBell {
        let visual_bell_config = &config.visual_bell;
        VisualBell {
            animation: visual_bell_config.animation,
            duration: visual_bell_config.duration(),
            start_time: None,
        }
    }

    /// Ring the visual bell, and return its intensity.
    pub fn ring(&mut self) -> f64 {
        let now = Instant::now();
        self.start_time = Some(now);
        self.intensity_at_instant(now)
    }

    /// Get the currently intensity of the visual bell. The bell's intensity
    /// ramps down from 1.0 to 0.0 at a rate determined by the bell's duration.
    pub fn intensity(&self) -> f64 {
        self.intensity_at_instant(Instant::now())
    }

    /// Check whether or not the visual bell has completed "ringing".
    pub fn completed(&mut self) -> bool {
        match self.start_time {
            Some(earlier) => {
                if Instant::now().duration_since(earlier) >= self.duration {
                    self.start_time = None;
                }
                false
            },
            None => true,
        }
    }

    /// Get the intensity of the visual bell at a particular instant. The bell's
    /// intensity ramps down from 1.0 to 0.0 at a rate determined by the bell's
    /// duration.
    pub fn intensity_at_instant(&self, instant: Instant) -> f64 {
        // If `duration` is zero, then the VisualBell is disabled; therefore,
        // its `intensity` is zero.
        if self.duration == Duration::from_secs(0) {
            return 0.0;
        }

        match self.start_time {
            // Similarly, if `start_time` is `None`, then the VisualBell has not
            // been "rung"; therefore, its `intensity` is zero.
            None => 0.0,

            Some(earlier) => {
                // Finally, if the `instant` at which we wish to compute the
                // VisualBell's `intensity` occurred before the VisualBell was
                // "rung", then its `intensity` is also zero.
                if instant < earlier {
                    return 0.0;
                }

                let elapsed = instant.duration_since(earlier);
                let elapsed_f =
                    elapsed.as_secs() as f64 + f64::from(elapsed.subsec_nanos()) / 1e9f64;
                let duration_f = self.duration.as_secs() as f64
                    + f64::from(self.duration.subsec_nanos()) / 1e9f64;

                // Otherwise, we compute a value `time` from 0.0 to 1.0
                // inclusive that represents the ratio of `elapsed` time to the
                // `duration` of the VisualBell.
                let time = (elapsed_f / duration_f).min(1.0);

                // We use this to compute the inverse `intensity` of the
                // VisualBell. When `time` is 0.0, `inverse_intensity` is 0.0,
                // and when `time` is 1.0, `inverse_intensity` is 1.0.
                let inverse_intensity = match self.animation {
                    VisualBellAnimation::Ease | VisualBellAnimation::EaseOut => {
                        cubic_bezier(0.25, 0.1, 0.25, 1.0, time)
                    },
                    VisualBellAnimation::EaseOutSine => cubic_bezier(0.39, 0.575, 0.565, 1.0, time),
                    VisualBellAnimation::EaseOutQuad => cubic_bezier(0.25, 0.46, 0.45, 0.94, time),
                    VisualBellAnimation::EaseOutCubic => {
                        cubic_bezier(0.215, 0.61, 0.355, 1.0, time)
                    },
                    VisualBellAnimation::EaseOutQuart => cubic_bezier(0.165, 0.84, 0.44, 1.0, time),
                    VisualBellAnimation::EaseOutQuint => cubic_bezier(0.23, 1.0, 0.32, 1.0, time),
                    VisualBellAnimation::EaseOutExpo => cubic_bezier(0.19, 1.0, 0.22, 1.0, time),
                    VisualBellAnimation::EaseOutCirc => cubic_bezier(0.075, 0.82, 0.165, 1.0, time),
                    VisualBellAnimation::Linear => time,
                };

                // Since we want the `intensity` of the VisualBell to decay over
                // `time`, we subtract the `inverse_intensity` from 1.0.
                1.0 - inverse_intensity
            },
        }
    }

    pub fn update_config<C>(&mut self, config: &Config<C>) {
        let visual_bell_config = &config.visual_bell;
        self.animation = visual_bell_config.animation;
        self.duration = visual_bell_config.duration();
    }
}

/// Terminal size info.
#[derive(Serialize, Deserialize, Debug, Copy, Clone, PartialEq)]
pub struct SizeInfo {
    /// Terminal window width.
    pub width: f32,

    /// Terminal window height.
    pub height: f32,

    /// Width of individual cell.
    pub cell_width: f32,

    /// Height of individual cell.
    pub cell_height: f32,

    /// Horizontal window padding.
    pub padding_x: f32,

    /// Horizontal window padding.
    pub padding_y: f32,

    /// DPR of the current window.
    #[serde(default)]
    pub dpr: f64,
}

impl SizeInfo {
    #[inline]
    pub fn lines(&self) -> Line {
        Line(((self.height - 2. * self.padding_y) / self.cell_height) as usize)
    }

    #[inline]
    pub fn cols(&self) -> Column {
        Column(((self.width - 2. * self.padding_x) / self.cell_width) as usize)
    }

    #[inline]
    pub fn padding_right(&self) -> usize {
        (self.padding_x + (self.width - 2. * self.padding_x) % self.cell_width) as usize
    }

    #[inline]
    pub fn padding_bottom(&self) -> usize {
        (self.padding_y + (self.height - 2. * self.padding_y) % self.cell_height) as usize
    }

    /// Check if coordinates are inside the terminal grid.
    ///
    /// The padding is not counted as part of the grid.
    #[inline]
    pub fn contains_point(&self, x: usize, y: usize) -> bool {
        x < (self.width as usize - self.padding_right())
            && x >= self.padding_x as usize
            && y < (self.height as usize - self.padding_bottom())
            && y >= self.padding_y as usize
    }

    pub fn pixels_to_coords(&self, x: usize, y: usize) -> Point {
        let col = Column(x.saturating_sub(self.padding_x as usize) / (self.cell_width as usize));
        let line = Line(y.saturating_sub(self.padding_y as usize) / (self.cell_height as usize));

        Point {
            line: min(line, Line(self.lines().saturating_sub(1))),
            col: min(col, Column(self.cols().saturating_sub(1))),
        }
    }
}

pub struct Term<T> {
    /// Terminal requires redraw.
    pub dirty: bool,

    /// Visual bell configuration and status.
    pub visual_bell: VisualBell,

    /// Terminal focus controlling the cursor shape.
    pub is_focused: bool,

    /// Cursor for keyboard selection.
    pub vi_mode_cursor: ViModeCursor,

    pub selection: Option<Selection>,

    /// Tracks if the next call to input will need to first handle wrapping.
    /// This is true after the last column is set with the input function. Any function that
    /// implicitly sets the line or column needs to set this to false to avoid wrapping twice.
    /// input_needs_wrap ensures that cursor.col is always valid for use into indexing into
    /// arrays. Without it we would have to sanitize cursor.col every time we used it.
    input_needs_wrap: bool,

    /// Currently active grid.
    ///
    /// Tracks the screen buffer currently in use. While the alternate screen buffer is active,
    /// this will be the alternate grid. Otherwise it is the primary screen buffer.
    grid: Grid<Cell>,

    /// Currently inactive grid.
    ///
    /// Opposite of the active grid. While the alternate screen buffer is active, this will be the
    /// primary grid. Otherwise it is the alternate screen buffer.
    inactive_grid: Grid<Cell>,

    /// Index into `charsets`, pointing to what ASCII is currently being mapped to.
    active_charset: CharsetIndex,

    /// Tabstops.
    tabs: TabStops,

    /// Mode flags.
    mode: TermMode,

    /// Scroll region.
    ///
    /// Range going from top to bottom of the terminal, indexed from the top of the viewport.
    scroll_region: Range<Line>,

    semantic_escape_chars: String,

    /// Colors used for rendering.
    colors: color::List,

    /// Is color in `colors` modified or not.
    color_modified: [bool; color::COUNT],

    /// Original colors from config.
    original_colors: color::List,

    /// Current style of the cursor.
    cursor_style: Option<CursorStyle>,

    /// Default style for resetting the cursor.
    default_cursor_style: CursorStyle,

    /// Style of the vi mode cursor.
    vi_mode_cursor_style: Option<CursorStyle>,

    /// Proxy for sending events to the event loop.
    event_proxy: T,

    /// Current title of the window.
    title: Option<String>,

    /// Default title for resetting it.
    default_title: String,

    /// Whether to permit updating the terminal title.
    dynamic_title: bool,

    /// Stack of saved window titles. When a title is popped from this stack, the `title` for the
    /// term is set, and the Glutin window's title attribute is changed through the event listener.
    title_stack: Vec<Option<String>>,
}

impl<T> Term<T> {
    #[inline]
    pub fn scroll_display(&mut self, scroll: Scroll)
    where
        T: EventListener,
    {
        self.event_proxy.send_event(Event::MouseCursorDirty);
        self.grid.scroll_display(scroll);
        self.dirty = true;
    }

    pub fn new<C>(config: &Config<C>, size: &SizeInfo, event_proxy: T) -> Term<T> {
        let num_cols = size.cols();
        let num_lines = size.lines();

        let history_size = config.scrolling.history() as usize;
        let grid = Grid::new(num_lines, num_cols, history_size, Cell::default());
        let alt = Grid::new(num_lines, num_cols, 0 /* scroll history */, Cell::default());

        let tabs = TabStops::new(grid.num_cols());

        let scroll_region = Line(0)..grid.num_lines();

        let colors = color::List::from(&config.colors);

        Term {
            dirty: false,
            visual_bell: VisualBell::new(config),
            input_needs_wrap: false,
            grid,
            inactive_grid: alt,
            active_charset: Default::default(),
            vi_mode_cursor: Default::default(),
            tabs,
            mode: Default::default(),
            scroll_region,
            colors,
            color_modified: [false; color::COUNT],
            original_colors: colors,
            semantic_escape_chars: config.selection.semantic_escape_chars().to_owned(),
            cursor_style: None,
            default_cursor_style: config.cursor.style,
            vi_mode_cursor_style: config.cursor.vi_mode_style,
            dynamic_title: config.dynamic_title(),
            event_proxy,
            is_focused: true,
            title: None,
            default_title: config.window.title.clone(),
            title_stack: Vec::new(),
            selection: None,
        }
    }

    pub fn update_config<C>(&mut self, config: &Config<C>)
    where
        T: EventListener,
    {
        self.semantic_escape_chars = config.selection.semantic_escape_chars().to_owned();
        self.original_colors.fill_named(&config.colors);
        self.original_colors.fill_cube(&config.colors);
        self.original_colors.fill_gray_ramp(&config.colors);
        for i in 0..color::COUNT {
            if !self.color_modified[i] {
                self.colors[i] = self.original_colors[i];
            }
        }
        self.visual_bell.update_config(config);
        if let Some(0) = config.scrolling.faux_multiplier() {
            self.mode.remove(TermMode::ALTERNATE_SCROLL);
        }
        self.default_cursor_style = config.cursor.style;
        self.vi_mode_cursor_style = config.cursor.vi_mode_style;

        self.default_title = config.window.title.clone();
        self.dynamic_title = config.dynamic_title();

        if self.dynamic_title {
            self.set_title(self.title.clone());
        } else {
            self.event_proxy.send_event(Event::Title(self.default_title.clone()));
        }

        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid.update_history(config.scrolling.history() as usize);
        } else {
            self.grid.update_history(config.scrolling.history() as usize);
        }
    }

    /// Convert the active selection to a String.
    pub fn selection_to_string(&self) -> Option<String> {
        let selection_range = self.selection.as_ref().and_then(|s| s.to_range(self))?;
        let SelectionRange { start, end, is_block } = selection_range;

        let mut res = String::new();

        if is_block {
            for line in (end.line + 1..=start.line).rev() {
                res += &self.line_to_string(line, start.col..end.col, start.col.0 != 0);

                // If the last column is included, newline is appended automatically.
                if end.col != self.cols() - 1 {
                    res += "\n";
                }
            }
            res += &self.line_to_string(end.line, start.col..end.col, true);
        } else {
            res = self.bounds_to_string(start, end);
        }

        Some(res)
    }

    /// Convert range between two points to a String.
    pub fn bounds_to_string(&self, start: Point<usize>, end: Point<usize>) -> String {
        let mut res = String::new();

        for line in (end.line..=start.line).rev() {
            let start_col = if line == start.line { start.col } else { Column(0) };
            let end_col = if line == end.line { end.col } else { self.cols() - 1 };

            res += &self.line_to_string(line, start_col..end_col, line == end.line);
        }

        res
    }

    /// Convert a single line in the grid to a String.
    fn line_to_string(
        &self,
        line: usize,
        mut cols: Range<Column>,
        include_wrapped_wide: bool,
    ) -> String {
        let mut text = String::new();

        let grid_line = &self.grid[line];
        let line_length = min(grid_line.line_length(), cols.end + 1);

        // Include wide char when trailing spacer is selected.
        if grid_line[cols.start].flags.contains(Flags::WIDE_CHAR_SPACER) {
            cols.start -= 1;
        }

        let mut tab_mode = false;
        for col in IndexRange::from(cols.start..line_length) {
            let cell = grid_line[col];

            // Skip over cells until next tab-stop once a tab was found.
            if tab_mode {
                if self.tabs[col] {
                    tab_mode = false;
                } else {
                    continue;
                }
            }

            if cell.c == '\t' {
                tab_mode = true;
            }

            if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                // Push cells primary character.
                text.push(cell.c);

                // Push zero-width characters.
                for c in (&cell.chars()[1..]).iter().take_while(|c| **c != ' ') {
                    text.push(*c);
                }
            }
        }

        if cols.end >= self.cols() - 1
            && (line_length.0 == 0
                || !self.grid[line][line_length - 1].flags.contains(Flags::WRAPLINE))
        {
            text.push('\n');
        }

        // If wide char is not part of the selection, but leading spacer is, include it.
        if line_length == self.grid.num_cols()
            && line_length.0 >= 2
            && grid_line[line_length - 1].flags.contains(Flags::WIDE_CHAR_SPACER)
            && !grid_line[line_length - 2].flags.contains(Flags::WIDE_CHAR)
            && include_wrapped_wide
        {
            text.push(self.grid[line - 1][Column(0)].c);
        }

        text
    }

    pub fn visible_to_buffer(&self, point: Point) -> Point<usize> {
        self.grid.visible_to_buffer(point)
    }

    /// Access to the raw grid data structure.
    ///
    /// This is a bit of a hack; when the window is closed, the event processor
    /// serializes the grid state to a file.
    pub fn grid(&self) -> &Grid<Cell> {
        &self.grid
    }

    /// Mutable access for swapping out the grid during tests.
    #[cfg(test)]
    pub fn grid_mut(&mut self) -> &mut Grid<Cell> {
        &mut self.grid
    }

    /// Iterate over the *renderable* cells in the terminal.
    ///
    /// A renderable cell is any cell which has content other than the default
    /// background color.  Cells with an alternate background color are
    /// considered renderable as are cells with any text content.
    pub fn renderable_cells<'b, C>(&'b self, config: &'b Config<C>) -> RenderableCellsIter<'_, C> {
        let selection = self.selection.as_ref().and_then(|s| s.to_range(self));

        RenderableCellsIter::new(&self, config, selection)
    }

    /// Resize terminal to new dimensions.
    pub fn resize(&mut self, size: &SizeInfo) {
        let old_cols = self.grid.num_cols();
        let old_lines = self.grid.num_lines();
        let num_cols = max(size.cols(), Column(MIN_SIZE));
        let num_lines = max(size.lines(), Line(MIN_SIZE));

        if old_cols == num_cols && old_lines == num_lines {
            debug!("Term::resize dimensions unchanged");
            return;
        }

        debug!("New num_cols is {} and num_lines is {}", num_cols, num_lines);

        let is_alt = self.mode.contains(TermMode::ALT_SCREEN);

        self.grid.resize(!is_alt, num_lines, num_cols);
        self.inactive_grid.resize(is_alt, num_lines, num_cols);

        // Clamp vi cursor to viewport.
        self.vi_mode_cursor.point.col = min(self.vi_mode_cursor.point.col, num_cols - 1);
        self.vi_mode_cursor.point.line = min(self.vi_mode_cursor.point.line, num_lines - 1);

        // Recreate tabs list.
        self.tabs.resize(self.grid.num_cols());

        // Reset scrolling region and selection.
        self.scroll_region = Line(0)..self.grid.num_lines();
        self.selection = None;
    }

    #[inline]
    pub fn mode(&self) -> &TermMode {
        &self.mode
    }

    /// Swap primary and alternate screen buffer.
    pub fn swap_alt(&mut self) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            let template = self.grid.cursor.template;
            self.grid.region_mut(..).each(|c| c.reset(&template));

            self.inactive_grid.cursor = self.inactive_grid.saved_cursor;
            self.grid.cursor = self.grid.saved_cursor;
        } else {
            self.inactive_grid.saved_cursor = self.inactive_grid.cursor;
            self.grid.saved_cursor = self.grid.cursor;
        }

        mem::swap(&mut self.grid, &mut self.inactive_grid);
        self.mode ^= TermMode::ALT_SCREEN;
        self.selection = None;
    }

    /// Scroll screen down.
    ///
    /// Text moves down; clear at bottom
    /// Expects origin to be in scroll range.
    #[inline]
    fn scroll_down_relative(&mut self, origin: Line, mut lines: Line) {
        trace!("Scrolling down relative: origin={}, lines={}", origin, lines);

        let num_lines = self.grid.num_lines();
        let num_cols = self.grid.num_cols();

        lines = min(lines, self.scroll_region.end - self.scroll_region.start);
        lines = min(lines, self.scroll_region.end - origin);

        let region = origin..self.scroll_region.end;
        let absolute_region = (num_lines - region.end)..(num_lines - region.start);

        // Scroll selection.
        self.selection = self
            .selection
            .take()
            .and_then(|s| s.rotate(num_lines, num_cols, &absolute_region, -(lines.0 as isize)));

        // Scroll between origin and bottom
        let template = Cell { bg: self.grid.cursor.template.bg, ..Cell::default() };
        self.grid.scroll_down(&region, lines, template);
    }

    /// Scroll screen up
    ///
    /// Text moves up; clear at top
    /// Expects origin to be in scroll range.
    #[inline]
    fn scroll_up_relative(&mut self, origin: Line, mut lines: Line) {
        trace!("Scrolling up relative: origin={}, lines={}", origin, lines);
        let num_lines = self.grid.num_lines();
        let num_cols = self.grid.num_cols();

        lines = min(lines, self.scroll_region.end - self.scroll_region.start);

        let region = origin..self.scroll_region.end;
        let absolute_region = (num_lines - region.end)..(num_lines - region.start);

        // Scroll selection.
        self.selection = self
            .selection
            .take()
            .and_then(|s| s.rotate(num_lines, num_cols, &absolute_region, lines.0 as isize));

        // Scroll from origin to bottom less number of lines.
        let template = Cell { bg: self.grid.cursor.template.bg, ..Cell::default() };
        self.grid.scroll_up(&region, lines, template);
    }

    fn deccolm(&mut self)
    where
        T: EventListener,
    {
        // Setting 132 column font makes no sense, but run the other side effects.
        // Clear scrolling region.
        self.set_scrolling_region(1, self.grid.num_lines().0);

        // Clear grid.
        let template = self.grid.cursor.template;
        self.grid.region_mut(..).each(|c| c.reset(&template));
    }

    #[inline]
    pub fn background_color(&self) -> Rgb {
        self.colors[NamedColor::Background]
    }

    #[inline]
    pub fn exit(&mut self)
    where
        T: EventListener,
    {
        self.event_proxy.send_event(Event::Exit);
    }

    /// Toggle the vi mode.
    #[inline]
    pub fn toggle_vi_mode(&mut self) {
        self.mode ^= TermMode::VI;
        self.selection = None;

        // Reset vi mode cursor position to match primary cursor.
        if self.mode.contains(TermMode::VI) {
            let cursor = self.grid.cursor.point;
            let line = min(cursor.line + self.grid.display_offset(), self.lines() - 1);
            self.vi_mode_cursor = ViModeCursor::new(Point::new(line, cursor.col));
        }

        self.dirty = true;
    }

    /// Move vi mode cursor.
    #[inline]
    pub fn vi_motion(&mut self, motion: ViMotion)
    where
        T: EventListener,
    {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Move cursor.
        self.vi_mode_cursor = self.vi_mode_cursor.motion(self, motion);

        // Update selection if one is active.
        let viewport_point = self.visible_to_buffer(self.vi_mode_cursor.point);
        if let Some(selection) = &mut self.selection {
            // Do not extend empty selections started by a single mouse click.
            if !selection.is_empty() {
                selection.update(viewport_point, Side::Left);
                selection.include_all();
            }
        }

        self.dirty = true;
    }

    #[inline]
    pub fn semantic_escape_chars(&self) -> &str {
        &self.semantic_escape_chars
    }

    /// Insert a linebreak at the current cursor position.
    #[inline]
    fn wrapline(&mut self)
    where
        T: EventListener,
    {
        if !self.mode.contains(TermMode::LINE_WRAP) {
            return;
        }

        trace!("Wrapping input");

        self.grid.cursor_cell().flags.insert(Flags::WRAPLINE);

        if (self.grid.cursor.point.line + 1) >= self.scroll_region.end {
            self.linefeed();
        } else {
            self.grid.cursor.point.line += 1;
        }

        self.grid.cursor.point.col = Column(0);
        self.input_needs_wrap = false;
    }

    /// Write `c` to the cell at the cursor position.
    #[inline]
    fn write_at_cursor(&mut self, c: char) -> &mut Cell
    where
        T: EventListener,
    {
        let mut cell = self.grid.cursor.template;
        cell.c = self.grid.cursor.charsets[self.active_charset].map(c);

        let cursor_cell = self.grid.cursor_cell();
        *cursor_cell = cell;

        cursor_cell
    }

    /// Get rendering information about the active cursor.
    fn renderable_cursor<C>(&self, config: &Config<C>) -> RenderableCursor {
        let vi_mode = self.mode.contains(TermMode::VI);

        // Cursor position.
        let mut point = if vi_mode {
            self.vi_mode_cursor.point
        } else {
            let mut point = self.grid.cursor.point;
            point.line += self.grid.display_offset();
            point
        };

        // Cursor shape.
        let hidden = !self.mode.contains(TermMode::SHOW_CURSOR) || point.line >= self.lines();
        let cursor_style = if hidden && !vi_mode {
            point.line = Line(0);
            CursorStyle::Hidden
        } else if !self.is_focused && config.cursor.unfocused_hollow() {
            CursorStyle::HollowBlock
        } else {
            let cursor_style = self.cursor_style.unwrap_or(self.default_cursor_style);

            if vi_mode {
                self.vi_mode_cursor_style.unwrap_or(cursor_style)
            } else {
                cursor_style
            }
        };

        // Cursor colors.
        let (text_color, cursor_color) = if vi_mode {
            (config.vi_mode_cursor_text_color(), config.vi_mode_cursor_cursor_color())
        } else {
            let cursor_cursor_color = config.cursor_cursor_color().map(|c| self.colors[c]);
            (config.cursor_text_color(), cursor_cursor_color)
        };

        // Expand across wide cell when inside wide char or spacer.
        let buffer_point = self.visible_to_buffer(point);
        let cell = self.grid[buffer_point.line][buffer_point.col];
        let is_wide = if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
            && self.grid[buffer_point.line][buffer_point.col - 1].flags.contains(Flags::WIDE_CHAR)
        {
            point.col -= 1;
            true
        } else {
            cell.flags.contains(Flags::WIDE_CHAR)
        };

        RenderableCursor {
            text_color,
            cursor_color,
            key: CursorKey { style: cursor_style, is_wide },
            point,
            rendered: false,
        }
    }
}

impl<T> TermInfo for Term<T> {
    #[inline]
    fn lines(&self) -> Line {
        self.grid.num_lines()
    }

    #[inline]
    fn cols(&self) -> Column {
        self.grid.num_cols()
    }
}

impl<T: EventListener> Handler for Term<T> {
    /// A character to be displayed.
    #[inline]
    fn input(&mut self, c: char) {
        // Number of cells the char will occupy.
        let width = match c.width() {
            Some(width) => width,
            None => return,
        };

        // Handle zero-width characters.
        if width == 0 {
            let mut col = self.grid.cursor.point.col.0.saturating_sub(1);
            let line = self.grid.cursor.point.line;
            if self.grid[line][Column(col)].flags.contains(Flags::WIDE_CHAR_SPACER) {
                col = col.saturating_sub(1);
            }
            self.grid[line][Column(col)].push_extra(c);
            return;
        }

        // Move cursor to next line.
        if self.input_needs_wrap {
            self.wrapline();
        }

        let num_cols = self.grid.num_cols();

        // If in insert mode, first shift cells to the right.
        if self.mode.contains(TermMode::INSERT) && self.grid.cursor.point.col + width < num_cols {
            let line = self.grid.cursor.point.line;
            let col = self.grid.cursor.point.col;
            let line = &mut self.grid[line];

            let src = line[col..].as_ptr();
            let dst = line[(col + width)..].as_mut_ptr();
            unsafe {
                ptr::copy(src, dst, (num_cols - col - width).0);
            }
        }

        if width == 1 {
            self.write_at_cursor(c);
        } else {
            if self.grid.cursor.point.col + 1 >= num_cols {
                if self.mode.contains(TermMode::LINE_WRAP) {
                    // Insert placeholder before wide char if glyph does not fit in this row.
                    self.write_at_cursor(' ').flags.insert(Flags::WIDE_CHAR_SPACER);
                    self.wrapline();
                } else {
                    // Prevent out of bounds crash when linewrapping is disabled.
                    self.input_needs_wrap = true;
                    return;
                }
            }

            // Write full width glyph to current cursor cell.
            self.write_at_cursor(c).flags.insert(Flags::WIDE_CHAR);

            // Write spacer to cell following the wide glyph.
            self.grid.cursor.point.col += 1;
            self.write_at_cursor(' ').flags.insert(Flags::WIDE_CHAR_SPACER);
        }

        if self.grid.cursor.point.col + 1 < num_cols {
            self.grid.cursor.point.col += 1;
        } else {
            self.input_needs_wrap = true;
        }
    }

    #[inline]
    fn decaln(&mut self) {
        trace!("Decalnning");

        let template = Cell { c: 'E', ..Cell::default() };
        self.grid.region_mut(..).each(|c| c.reset(&template));
    }

    #[inline]
    fn goto(&mut self, line: Line, col: Column) {
        trace!("Going to: line={}, col={}", line, col);
        let (y_offset, max_y) = if self.mode.contains(TermMode::ORIGIN) {
            (self.scroll_region.start, self.scroll_region.end - 1)
        } else {
            (Line(0), self.grid.num_lines() - 1)
        };

        self.grid.cursor.point.line = min(line + y_offset, max_y);
        self.grid.cursor.point.col = min(col, self.grid.num_cols() - 1);
        self.input_needs_wrap = false;
    }

    #[inline]
    fn goto_line(&mut self, line: Line) {
        trace!("Going to line: {}", line);
        self.goto(line, self.grid.cursor.point.col)
    }

    #[inline]
    fn goto_col(&mut self, col: Column) {
        trace!("Going to column: {}", col);
        self.goto(self.grid.cursor.point.line, col)
    }

    #[inline]
    fn insert_blank(&mut self, count: Column) {
        let cursor = self.grid.cursor;

        // Ensure inserting within terminal bounds
        let count = min(count, self.grid.num_cols() - cursor.point.col);

        let source = cursor.point.col;
        let destination = cursor.point.col + count;
        let num_cells = (self.grid.num_cols() - destination).0;

        let line = &mut self.grid[cursor.point.line];

        unsafe {
            let src = line[source..].as_ptr();
            let dst = line[destination..].as_mut_ptr();

            ptr::copy(src, dst, num_cells);
        }

        // Cells were just moved out towards the end of the line; fill in
        // between source and dest with blanks.
        for c in &mut line[source..destination] {
            c.reset(&cursor.template);
        }
    }

    #[inline]
    fn move_up(&mut self, lines: Line) {
        trace!("Moving up: {}", lines);
        let move_to = Line(self.grid.cursor.point.line.0.saturating_sub(lines.0));
        self.goto(move_to, self.grid.cursor.point.col)
    }

    #[inline]
    fn move_down(&mut self, lines: Line) {
        trace!("Moving down: {}", lines);
        let move_to = self.grid.cursor.point.line + lines;
        self.goto(move_to, self.grid.cursor.point.col)
    }

    #[inline]
    fn move_forward(&mut self, cols: Column) {
        trace!("Moving forward: {}", cols);
        let num_cols = self.grid.num_cols();
        self.grid.cursor.point.col = min(self.grid.cursor.point.col + cols, num_cols - 1);
        self.input_needs_wrap = false;
    }

    #[inline]
    fn move_backward(&mut self, cols: Column) {
        trace!("Moving backward: {}", cols);
        self.grid.cursor.point.col = Column(self.grid.cursor.point.col.saturating_sub(cols.0));
        self.input_needs_wrap = false;
    }

    #[inline]
    fn identify_terminal<W: io::Write>(&mut self, writer: &mut W) {
        trace!("Reporting terminal identity");
        let _ = writer.write_all(b"\x1b[?6c");
    }

    #[inline]
    fn device_status<W: io::Write>(&mut self, writer: &mut W, arg: usize) {
        trace!("Reporting device status: {}", arg);
        match arg {
            5 => {
                let _ = writer.write_all(b"\x1b[0n");
            },
            6 => {
                let pos = self.grid.cursor.point;
                let response = format!("\x1b[{};{}R", pos.line + 1, pos.col + 1);
                let _ = writer.write_all(response.as_bytes());
            },
            _ => debug!("unknown device status query: {}", arg),
        };
    }

    #[inline]
    fn move_down_and_cr(&mut self, lines: Line) {
        trace!("Moving down and cr: {}", lines);
        let move_to = self.grid.cursor.point.line + lines;
        self.goto(move_to, Column(0))
    }

    #[inline]
    fn move_up_and_cr(&mut self, lines: Line) {
        trace!("Moving up and cr: {}", lines);
        let move_to = Line(self.grid.cursor.point.line.0.saturating_sub(lines.0));
        self.goto(move_to, Column(0))
    }

    /// Insert tab at cursor position.
    #[inline]
    fn put_tab(&mut self, mut count: i64) {
        // A tab after the last column is the same as a linebreak.
        if self.input_needs_wrap {
            self.wrapline();
            return;
        }

        while self.grid.cursor.point.col < self.grid.num_cols() && count != 0 {
            count -= 1;

            let c = self.grid.cursor.charsets[self.active_charset].map('\t');
            let cell = self.grid.cursor_cell();
            if cell.c == ' ' {
                cell.c = c;
            }

            loop {
                if (self.grid.cursor.point.col + 1) == self.grid.num_cols() {
                    break;
                }

                self.grid.cursor.point.col += 1;

                if self.tabs[self.grid.cursor.point.col] {
                    break;
                }
            }
        }
    }

    /// Backspace `count` characters.
    #[inline]
    fn backspace(&mut self) {
        trace!("Backspace");

        if self.grid.cursor.point.col > Column(0) {
            self.grid.cursor.point.col -= 1;
            self.input_needs_wrap = false;
        }
    }

    /// Carriage return.
    #[inline]
    fn carriage_return(&mut self) {
        trace!("Carriage return");
        self.grid.cursor.point.col = Column(0);
        self.input_needs_wrap = false;
    }

    /// Linefeed.
    #[inline]
    fn linefeed(&mut self) {
        trace!("Linefeed");
        let next = self.grid.cursor.point.line + 1;
        if next == self.scroll_region.end {
            self.scroll_up(Line(1));
        } else if next < self.grid.num_lines() {
            self.grid.cursor.point.line += 1;
        }
    }

    /// Set current position as a tabstop.
    #[inline]
    fn bell(&mut self) {
        trace!("Bell");
        self.visual_bell.ring();
        self.event_proxy.send_event(Event::Urgent);
    }

    #[inline]
    fn substitute(&mut self) {
        trace!("[unimplemented] Substitute");
    }

    /// Run LF/NL.
    ///
    /// LF/NL mode has some interesting history. According to ECMA-48 4th
    /// edition, in LINE FEED mode,
    ///
    /// > The execution of the formatter functions LINE FEED (LF), FORM FEED
    /// (FF), LINE TABULATION (VT) cause only movement of the active position in
    /// the direction of the line progression.
    ///
    /// In NEW LINE mode,
    ///
    /// > The execution of the formatter functions LINE FEED (LF), FORM FEED
    /// (FF), LINE TABULATION (VT) cause movement to the line home position on
    /// the following line, the following form, etc. In the case of LF this is
    /// referred to as the New Line (NL) option.
    ///
    /// Additionally, ECMA-48 4th edition says that this option is deprecated.
    /// ECMA-48 5th edition only mentions this option (without explanation)
    /// saying that it's been removed.
    ///
    /// As an emulator, we need to support it since applications may still rely
    /// on it.
    #[inline]
    fn newline(&mut self) {
        self.linefeed();

        if self.mode.contains(TermMode::LINE_FEED_NEW_LINE) {
            self.carriage_return();
        }
    }

    #[inline]
    fn set_horizontal_tabstop(&mut self) {
        trace!("Setting horizontal tabstop");
        self.tabs[self.grid.cursor.point.col] = true;
    }

    #[inline]
    fn scroll_up(&mut self, lines: Line) {
        let origin = self.scroll_region.start;
        self.scroll_up_relative(origin, lines);
    }

    #[inline]
    fn scroll_down(&mut self, lines: Line) {
        let origin = self.scroll_region.start;
        self.scroll_down_relative(origin, lines);
    }

    #[inline]
    fn insert_blank_lines(&mut self, lines: Line) {
        trace!("Inserting blank {} lines", lines);

        let origin = self.grid.cursor.point.line;
        if self.scroll_region.contains(&origin) {
            self.scroll_down_relative(origin, lines);
        }
    }

    #[inline]
    fn delete_lines(&mut self, lines: Line) {
        let origin = self.grid.cursor.point.line;
        let lines = min(self.lines() - origin, lines);

        trace!("Deleting {} lines", lines);

        if lines.0 > 0 && self.scroll_region.contains(&self.grid.cursor.point.line) {
            self.scroll_up_relative(origin, lines);
        }
    }

    #[inline]
    fn erase_chars(&mut self, count: Column) {
        let cursor = self.grid.cursor;

        trace!("Erasing chars: count={}, col={}", count, cursor.point.col);

        let start = cursor.point.col;
        let end = min(start + count, self.grid.num_cols());

        // Cleared cells have current background color set.
        let row = &mut self.grid[cursor.point.line];
        for c in &mut row[start..end] {
            c.reset(&cursor.template);
        }
    }

    #[inline]
    fn delete_chars(&mut self, count: Column) {
        let cols = self.grid.num_cols();
        let cursor = self.grid.cursor;

        // Ensure deleting within terminal bounds.
        let count = min(count, cols);

        let start = cursor.point.col;
        let end = min(start + count, cols - 1);
        let n = (cols - end).0;

        let line = &mut self.grid[cursor.point.line];

        unsafe {
            let src = line[end..].as_ptr();
            let dst = line[start..].as_mut_ptr();

            ptr::copy(src, dst, n);
        }

        // Clear last `count` cells in line. If deleting 1 char, need to delete
        // 1 cell.
        let end = cols - count;
        for c in &mut line[end..] {
            c.reset(&cursor.template);
        }
    }

    #[inline]
    fn move_backward_tabs(&mut self, count: i64) {
        trace!("Moving backward {} tabs", count);

        for _ in 0..count {
            let mut col = self.grid.cursor.point.col;
            for i in (0..(col.0)).rev() {
                if self.tabs[index::Column(i)] {
                    col = index::Column(i);
                    break;
                }
            }
            self.grid.cursor.point.col = col;
        }
    }

    #[inline]
    fn move_forward_tabs(&mut self, count: i64) {
        trace!("[unimplemented] Moving forward {} tabs", count);
    }

    #[inline]
    fn save_cursor_position(&mut self) {
        trace!("Saving cursor position");

        self.grid.saved_cursor = self.grid.cursor;
    }

    #[inline]
    fn restore_cursor_position(&mut self) {
        trace!("Restoring cursor position");

        self.grid.cursor = self.grid.saved_cursor;
    }

    #[inline]
    fn clear_line(&mut self, mode: ansi::LineClearMode) {
        trace!("Clearing line: {:?}", mode);

        let cursor = self.grid.cursor;
        match mode {
            ansi::LineClearMode::Right => {
                let row = &mut self.grid[cursor.point.line];
                for cell in &mut row[cursor.point.col..] {
                    cell.reset(&cursor.template);
                }
            },
            ansi::LineClearMode::Left => {
                let row = &mut self.grid[cursor.point.line];
                for cell in &mut row[..=cursor.point.col] {
                    cell.reset(&cursor.template);
                }
            },
            ansi::LineClearMode::All => {
                let row = &mut self.grid[cursor.point.line];
                for cell in &mut row[..] {
                    cell.reset(&cursor.template);
                }
            },
        }

        let cursor_buffer_line = (self.grid.num_lines() - self.grid.cursor.point.line - 1).0;
        self.selection = self
            .selection
            .take()
            .filter(|s| !s.intersects_range(cursor_buffer_line..=cursor_buffer_line));
    }

    /// Set the indexed color value.
    #[inline]
    fn set_color(&mut self, index: usize, color: Rgb) {
        trace!("Setting color[{}] = {:?}", index, color);
        self.colors[index] = color;
        self.color_modified[index] = true;
    }

    /// Write a foreground/background color escape sequence with the current color.
    #[inline]
    fn dynamic_color_sequence<W: io::Write>(
        &mut self,
        writer: &mut W,
        code: u8,
        index: usize,
        terminator: &str,
    ) {
        trace!("Writing escape sequence for dynamic color code {}: color[{}]", code, index);
        let color = self.colors[index];
        let response = format!(
            "\x1b]{};rgb:{1:02x}{1:02x}/{2:02x}{2:02x}/{3:02x}{3:02x}{4}",
            code, color.r, color.g, color.b, terminator
        );
        let _ = writer.write_all(response.as_bytes());
    }

    /// Reset the indexed color to original value.
    #[inline]
    fn reset_color(&mut self, index: usize) {
        trace!("Resetting color[{}]", index);
        self.colors[index] = self.original_colors[index];
        self.color_modified[index] = false;
    }

    /// Store data into clipboard.
    #[inline]
    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        let clipboard_type = match clipboard {
            b'c' => ClipboardType::Clipboard,
            b'p' | b's' => ClipboardType::Selection,
            _ => return,
        };

        if let Ok(bytes) = base64::decode(base64) {
            if let Ok(text) = String::from_utf8(bytes) {
                self.event_proxy.send_event(Event::ClipboardStore(clipboard_type, text));
            }
        }
    }

    /// Load data from clipboard.
    #[inline]
    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        let clipboard_type = match clipboard {
            b'c' => ClipboardType::Clipboard,
            b'p' | b's' => ClipboardType::Selection,
            _ => return,
        };

        let terminator = terminator.to_owned();

        self.event_proxy.send_event(Event::ClipboardLoad(
            clipboard_type,
            Arc::new(move |text| {
                let base64 = base64::encode(&text);
                format!("\x1b]52;{};{}{}", clipboard as char, base64, terminator)
            }),
        ));
    }

    #[inline]
    fn clear_screen(&mut self, mode: ansi::ClearMode) {
        trace!("Clearing screen: {:?}", mode);
        let template = self.grid.cursor.template;

        let num_lines = self.grid.num_lines().0;
        let cursor_buffer_line = num_lines - self.grid.cursor.point.line.0 - 1;

        match mode {
            ansi::ClearMode::Above => {
                let cursor = self.grid.cursor.point;

                // If clearing more than one line.
                if cursor.line > Line(1) {
                    // Fully clear all lines before the current line.
                    self.grid.region_mut(..cursor.line).each(|cell| cell.reset(&template));
                }

                // Clear up to the current column in the current line.
                let end = min(cursor.col + 1, self.grid.num_cols());
                for cell in &mut self.grid[cursor.line][..end] {
                    cell.reset(&template);
                }

                self.selection = self
                    .selection
                    .take()
                    .filter(|s| !s.intersects_range(cursor_buffer_line..num_lines));
            },
            ansi::ClearMode::Below => {
                let cursor = self.grid.cursor.point;
                for cell in &mut self.grid[cursor.line][cursor.col..] {
                    cell.reset(&template);
                }

                if cursor.line.0 < num_lines - 1 {
                    self.grid.region_mut((cursor.line + 1)..).each(|cell| cell.reset(&template));
                }

                self.selection =
                    self.selection.take().filter(|s| !s.intersects_range(..=cursor_buffer_line));
            },
            ansi::ClearMode::All => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.grid.region_mut(..).each(|c| c.reset(&template));
                } else {
                    let template = Cell { bg: template.bg, ..Cell::default() };
                    self.grid.clear_viewport(template);
                }

                self.selection = self.selection.take().filter(|s| !s.intersects_range(..num_lines));
            },
            ansi::ClearMode::Saved if self.grid.history_size() > 0 => {
                self.grid.clear_history();

                self.selection = self.selection.take().filter(|s| !s.intersects_range(num_lines..));
            },
            // We have no history to clear.
            ansi::ClearMode::Saved => (),
        }
    }

    #[inline]
    fn clear_tabs(&mut self, mode: ansi::TabulationClearMode) {
        trace!("Clearing tabs: {:?}", mode);
        match mode {
            ansi::TabulationClearMode::Current => {
                self.tabs[self.grid.cursor.point.col] = false;
            },
            ansi::TabulationClearMode::All => {
                self.tabs.clear_all();
            },
        }
    }

    /// Reset all important fields in the term struct.
    #[inline]
    fn reset_state(&mut self) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            mem::swap(&mut self.grid, &mut self.inactive_grid);
        }
        self.input_needs_wrap = false;
        self.active_charset = Default::default();
        self.mode = Default::default();
        self.colors = self.original_colors;
        self.color_modified = [false; color::COUNT];
        self.cursor_style = None;
        self.grid.reset(Cell::default());
        self.inactive_grid.reset(Cell::default());
        self.scroll_region = Line(0)..self.grid.num_lines();
        self.tabs = TabStops::new(self.grid.num_cols());
        self.title_stack = Vec::new();
        self.title = None;
        self.selection = None;
    }

    #[inline]
    fn reverse_index(&mut self) {
        trace!("Reversing index");

        if self.grid.cursor.point.line == self.scroll_region.start {
            self.scroll_down(Line(1));
        } else {
            self.grid.cursor.point.line = Line(self.grid.cursor.point.line.saturating_sub(1));
        }
    }

    /// Set a terminal attribute.
    #[inline]
    fn terminal_attribute(&mut self, attr: Attr) {
        trace!("Setting attribute: {:?}", attr);
        let cursor = &mut self.grid.cursor;
        match attr {
            Attr::Foreground(color) => cursor.template.fg = color,
            Attr::Background(color) => cursor.template.bg = color,
            Attr::Reset => {
                cursor.template.fg = Color::Named(NamedColor::Foreground);
                cursor.template.bg = Color::Named(NamedColor::Background);
                cursor.template.flags = Flags::empty();
            },
            Attr::Reverse => cursor.template.flags.insert(Flags::INVERSE),
            Attr::CancelReverse => cursor.template.flags.remove(Flags::INVERSE),
            Attr::Bold => cursor.template.flags.insert(Flags::BOLD),
            Attr::CancelBold => cursor.template.flags.remove(Flags::BOLD),
            Attr::Dim => cursor.template.flags.insert(Flags::DIM),
            Attr::CancelBoldDim => cursor.template.flags.remove(Flags::BOLD | Flags::DIM),
            Attr::Italic => cursor.template.flags.insert(Flags::ITALIC),
            Attr::CancelItalic => cursor.template.flags.remove(Flags::ITALIC),
            Attr::Underline => cursor.template.flags.insert(Flags::UNDERLINE),
            Attr::CancelUnderline => cursor.template.flags.remove(Flags::UNDERLINE),
            Attr::Hidden => cursor.template.flags.insert(Flags::HIDDEN),
            Attr::CancelHidden => cursor.template.flags.remove(Flags::HIDDEN),
            Attr::Strike => cursor.template.flags.insert(Flags::STRIKEOUT),
            Attr::CancelStrike => cursor.template.flags.remove(Flags::STRIKEOUT),
            _ => {
                debug!("Term got unhandled attr: {:?}", attr);
            },
        }
    }

    #[inline]
    fn set_mode(&mut self, mode: ansi::Mode) {
        trace!("Setting mode: {:?}", mode);
        match mode {
            ansi::Mode::SwapScreenAndSetRestoreCursor => {
                if !self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt();
                }
            },
            ansi::Mode::ShowCursor => self.mode.insert(TermMode::SHOW_CURSOR),
            ansi::Mode::CursorKeys => self.mode.insert(TermMode::APP_CURSOR),
            // Mouse protocols are mutually exclusive.
            ansi::Mode::ReportMouseClicks => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_REPORT_CLICK);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            ansi::Mode::ReportCellMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_DRAG);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            ansi::Mode::ReportAllMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_MOTION);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            ansi::Mode::ReportFocusInOut => self.mode.insert(TermMode::FOCUS_IN_OUT),
            ansi::Mode::BracketedPaste => self.mode.insert(TermMode::BRACKETED_PASTE),
            // Mouse encodings are mutually exclusive.
            ansi::Mode::SgrMouse => {
                self.mode.remove(TermMode::UTF8_MOUSE);
                self.mode.insert(TermMode::SGR_MOUSE);
            },
            ansi::Mode::Utf8Mouse => {
                self.mode.remove(TermMode::SGR_MOUSE);
                self.mode.insert(TermMode::UTF8_MOUSE);
            },
            ansi::Mode::AlternateScroll => self.mode.insert(TermMode::ALTERNATE_SCROLL),
            ansi::Mode::LineWrap => self.mode.insert(TermMode::LINE_WRAP),
            ansi::Mode::LineFeedNewLine => self.mode.insert(TermMode::LINE_FEED_NEW_LINE),
            ansi::Mode::Origin => self.mode.insert(TermMode::ORIGIN),
            ansi::Mode::DECCOLM => self.deccolm(),
            ansi::Mode::Insert => self.mode.insert(TermMode::INSERT),
            ansi::Mode::BlinkingCursor => {
                trace!("... unimplemented mode");
            },
        }
    }

    #[inline]
    fn unset_mode(&mut self, mode: ansi::Mode) {
        trace!("Unsetting mode: {:?}", mode);
        match mode {
            ansi::Mode::SwapScreenAndSetRestoreCursor => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt();
                }
            },
            ansi::Mode::ShowCursor => self.mode.remove(TermMode::SHOW_CURSOR),
            ansi::Mode::CursorKeys => self.mode.remove(TermMode::APP_CURSOR),
            ansi::Mode::ReportMouseClicks => {
                self.mode.remove(TermMode::MOUSE_REPORT_CLICK);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            ansi::Mode::ReportCellMouseMotion => {
                self.mode.remove(TermMode::MOUSE_DRAG);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            ansi::Mode::ReportAllMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MOTION);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            ansi::Mode::ReportFocusInOut => self.mode.remove(TermMode::FOCUS_IN_OUT),
            ansi::Mode::BracketedPaste => self.mode.remove(TermMode::BRACKETED_PASTE),
            ansi::Mode::SgrMouse => self.mode.remove(TermMode::SGR_MOUSE),
            ansi::Mode::Utf8Mouse => self.mode.remove(TermMode::UTF8_MOUSE),
            ansi::Mode::AlternateScroll => self.mode.remove(TermMode::ALTERNATE_SCROLL),
            ansi::Mode::LineWrap => self.mode.remove(TermMode::LINE_WRAP),
            ansi::Mode::LineFeedNewLine => self.mode.remove(TermMode::LINE_FEED_NEW_LINE),
            ansi::Mode::Origin => self.mode.remove(TermMode::ORIGIN),
            ansi::Mode::DECCOLM => self.deccolm(),
            ansi::Mode::Insert => self.mode.remove(TermMode::INSERT),
            ansi::Mode::BlinkingCursor => {
                trace!("... unimplemented mode");
            },
        }
    }

    #[inline]
    fn set_scrolling_region(&mut self, top: usize, bottom: usize) {
        if top >= bottom {
            debug!("Invalid scrolling region: ({};{})", top, bottom);
            return;
        }

        // Bottom should be included in the range, but range end is not
        // usually included. One option would be to use an inclusive
        // range, but instead we just let the open range end be 1
        // higher.
        let start = Line(top - 1);
        let end = Line(bottom);

        trace!("Setting scrolling region: ({};{})", start, end);

        self.scroll_region.start = min(start, self.grid.num_lines());
        self.scroll_region.end = min(end, self.grid.num_lines());
        self.goto(Line(0), Column(0));
    }

    #[inline]
    fn set_keypad_application_mode(&mut self) {
        trace!("Setting keypad application mode");
        self.mode.insert(TermMode::APP_KEYPAD);
    }

    #[inline]
    fn unset_keypad_application_mode(&mut self) {
        trace!("Unsetting keypad application mode");
        self.mode.remove(TermMode::APP_KEYPAD);
    }

    #[inline]
    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        trace!("Configuring charset {:?} as {:?}", index, charset);
        self.grid.cursor.charsets[index] = charset;
    }

    #[inline]
    fn set_active_charset(&mut self, index: CharsetIndex) {
        trace!("Setting active charset {:?}", index);
        self.active_charset = index;
    }

    #[inline]
    fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        trace!("Setting cursor style {:?}", style);
        self.cursor_style = style;
    }

    #[inline]
    fn set_title(&mut self, title: Option<String>) {
        trace!("Setting title to '{:?}'", title);

        self.title = title.clone();

        if self.dynamic_title {
            let title = title.unwrap_or_else(|| self.default_title.clone());
            self.event_proxy.send_event(Event::Title(title));
        }
    }

    #[inline]
    fn push_title(&mut self) {
        trace!("Pushing '{:?}' onto title stack", self.title);

        if self.title_stack.len() >= TITLE_STACK_MAX_DEPTH {
            let removed = self.title_stack.remove(0);
            trace!(
                "Removing '{:?}' from bottom of title stack that exceeds its maximum depth",
                removed
            );
        }

        self.title_stack.push(self.title.clone());
    }

    #[inline]
    fn pop_title(&mut self) {
        trace!("Attempting to pop title from stack...");

        if let Some(popped) = self.title_stack.pop() {
            trace!("Title '{:?}' popped from stack", popped);
            self.set_title(popped);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardType {
    Clipboard,
    Selection,
}

struct TabStops {
    tabs: Vec<bool>,
}

impl TabStops {
    #[inline]
    fn new(num_cols: Column) -> TabStops {
        TabStops {
            tabs: IndexRange::from(Column(0)..num_cols)
                .map(|i| (*i as usize) % INITIAL_TABSTOPS == 0)
                .collect::<Vec<bool>>(),
        }
    }

    /// Remove all tabstops.
    #[inline]
    fn clear_all(&mut self) {
        unsafe {
            ptr::write_bytes(self.tabs.as_mut_ptr(), 0, self.tabs.len());
        }
    }

    /// Increase tabstop capacity.
    #[inline]
    fn resize(&mut self, num_cols: Column) {
        let mut index = self.tabs.len();
        self.tabs.resize_with(num_cols.0, || {
            let is_tabstop = index % INITIAL_TABSTOPS == 0;
            index += 1;
            is_tabstop
        });
    }
}

impl Index<Column> for TabStops {
    type Output = bool;

    fn index(&self, index: Column) -> &bool {
        &self.tabs[index.0]
    }
}

impl IndexMut<Column> for TabStops {
    fn index_mut(&mut self, index: Column) -> &mut bool {
        self.tabs.index_mut(index.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::mem;

    use crate::ansi::{self, CharsetIndex, Handler, StandardCharset};
    use crate::config::MockConfig;
    use crate::event::{Event, EventListener};
    use crate::grid::{Grid, Scroll};
    use crate::index::{Column, Line, Point, Side};
    use crate::selection::{Selection, SelectionType};
    use crate::term::cell::{Cell, Flags};

    struct Mock;
    impl EventListener for Mock {
        fn send_event(&self, _event: Event) {}
    }

    #[test]
    fn semantic_selection_works() {
        let size = SizeInfo {
            width: 21.0,
            height: 51.0,
            cell_width: 3.0,
            cell_height: 3.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);
        let mut grid: Grid<Cell> = Grid::new(Line(3), Column(5), 0, Cell::default());
        for i in 0..5 {
            for j in 0..2 {
                grid[Line(j)][Column(i)].c = 'a';
            }
        }
        grid[Line(0)][Column(0)].c = '"';
        grid[Line(0)][Column(3)].c = '"';
        grid[Line(1)][Column(2)].c = '"';
        grid[Line(0)][Column(4)].flags.insert(Flags::WRAPLINE);

        let mut escape_chars = String::from("\"");

        mem::swap(&mut term.grid, &mut grid);
        mem::swap(&mut term.semantic_escape_chars, &mut escape_chars);

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: 2, col: Column(1) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aa")));
        }

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: 2, col: Column(4) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aaa")));
        }

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: 1, col: Column(1) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aaa")));
        }
    }

    #[test]
    fn line_selection_works() {
        let size = SizeInfo {
            width: 21.0,
            height: 51.0,
            cell_width: 3.0,
            cell_height: 3.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);
        let mut grid: Grid<Cell> = Grid::new(Line(1), Column(5), 0, Cell::default());
        for i in 0..5 {
            grid[Line(0)][Column(i)].c = 'a';
        }
        grid[Line(0)][Column(0)].c = '"';
        grid[Line(0)][Column(3)].c = '"';

        mem::swap(&mut term.grid, &mut grid);

        term.selection = Some(Selection::new(
            SelectionType::Lines,
            Point { line: 0, col: Column(3) },
            Side::Left,
        ));
        assert_eq!(term.selection_to_string(), Some(String::from("\"aa\"a\n")));
    }

    #[test]
    fn selecting_empty_line() {
        let size = SizeInfo {
            width: 21.0,
            height: 51.0,
            cell_width: 3.0,
            cell_height: 3.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);
        let mut grid: Grid<Cell> = Grid::new(Line(3), Column(3), 0, Cell::default());
        for l in 0..3 {
            if l != 1 {
                for c in 0..3 {
                    grid[Line(l)][Column(c)].c = 'a';
                }
            }
        }

        mem::swap(&mut term.grid, &mut grid);

        let mut selection =
            Selection::new(SelectionType::Simple, Point { line: 2, col: Column(0) }, Side::Left);
        selection.update(Point { line: 0, col: Column(2) }, Side::Right);
        term.selection = Some(selection);
        assert_eq!(term.selection_to_string(), Some("aaa\n\naaa\n".into()));
    }

    /// Check that the grid can be serialized back and forth losslessly.
    ///
    /// This test is in the term module as opposed to the grid since we want to
    /// test this property with a T=Cell.
    #[test]
    fn grid_serde() {
        let template = Cell::default();

        let grid: Grid<Cell> = Grid::new(Line(24), Column(80), 0, template);
        let serialized = serde_json::to_string(&grid).expect("ser");
        let deserialized = serde_json::from_str::<Grid<Cell>>(&serialized).expect("de");

        assert_eq!(deserialized, grid);
    }

    #[test]
    fn input_line_drawing_character() {
        let size = SizeInfo {
            width: 21.0,
            height: 51.0,
            cell_width: 3.0,
            cell_height: 3.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);
        let cursor = Point::new(Line(0), Column(0));
        term.configure_charset(CharsetIndex::G0, StandardCharset::SpecialCharacterAndLineDrawing);
        term.input('a');

        assert_eq!(term.grid()[&cursor].c, '▒');
    }

    #[test]
    fn clear_saved_lines() {
        let size = SizeInfo {
            width: 21.0,
            height: 51.0,
            cell_width: 3.0,
            cell_height: 3.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);

        // Add one line of scrollback.
        term.grid.scroll_up(&(Line(0)..Line(1)), Line(1), Cell::default());

        // Clear the history.
        term.clear_screen(ansi::ClearMode::Saved);

        // Make sure that scrolling does not change the grid.
        let mut scrolled_grid = term.grid.clone();
        scrolled_grid.scroll_display(Scroll::Top);

        // Truncate grids for comparison.
        scrolled_grid.truncate();
        term.grid.truncate();

        assert_eq!(term.grid, scrolled_grid);
    }

    #[test]
    fn grow_lines_updates_active_cursor_pos() {
        let mut size = SizeInfo {
            width: 100.0,
            height: 10.0,
            cell_width: 1.0,
            cell_height: 1.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.grid.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Increase visible lines.
        size.height = 30.;
        term.resize(&size);

        assert_eq!(term.grid.history_size(), 0);
        assert_eq!(term.grid.cursor.point, Point::new(Line(19), Column(0)));
    }

    #[test]
    fn grow_lines_updates_inactive_cursor_pos() {
        let mut size = SizeInfo {
            width: 100.0,
            height: 10.0,
            cell_width: 1.0,
            cell_height: 1.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.grid.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Enter alt screen.
        term.set_mode(ansi::Mode::SwapScreenAndSetRestoreCursor);

        // Increase visible lines.
        size.height = 30.;
        term.resize(&size);

        // Leave alt screen.
        term.unset_mode(ansi::Mode::SwapScreenAndSetRestoreCursor);

        assert_eq!(term.grid().history_size(), 0);
        assert_eq!(term.grid.cursor.point, Point::new(Line(19), Column(0)));
    }

    #[test]
    fn shrink_lines_updates_active_cursor_pos() {
        let mut size = SizeInfo {
            width: 100.0,
            height: 10.0,
            cell_width: 1.0,
            cell_height: 1.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.grid.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Increase visible lines.
        size.height = 5.;
        term.resize(&size);

        assert_eq!(term.grid().history_size(), 15);
        assert_eq!(term.grid.cursor.point, Point::new(Line(4), Column(0)));
    }

    #[test]
    fn shrink_lines_updates_inactive_cursor_pos() {
        let mut size = SizeInfo {
            width: 100.0,
            height: 10.0,
            cell_width: 1.0,
            cell_height: 1.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.grid.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Enter alt screen.
        term.set_mode(ansi::Mode::SwapScreenAndSetRestoreCursor);

        // Increase visible lines.
        size.height = 5.;
        term.resize(&size);

        // Leave alt screen.
        term.unset_mode(ansi::Mode::SwapScreenAndSetRestoreCursor);

        assert_eq!(term.grid().history_size(), 15);
        assert_eq!(term.grid.cursor.point, Point::new(Line(4), Column(0)));
    }

    #[test]
    fn window_title() {
        let size = SizeInfo {
            width: 21.0,
            height: 51.0,
            cell_width: 3.0,
            cell_height: 3.0,
            padding_x: 0.0,
            padding_y: 0.0,
            dpr: 1.0,
        };
        let mut term = Term::new(&MockConfig::default(), &size, Mock);

        // Title None by default.
        assert_eq!(term.title, None);

        // Title can be set.
        term.set_title(Some("Test".into()));
        assert_eq!(term.title, Some("Test".into()));

        // Title can be pushed onto stack.
        term.push_title();
        term.set_title(Some("Next".into()));
        assert_eq!(term.title, Some("Next".into()));
        assert_eq!(term.title_stack.get(0).unwrap(), &Some("Test".into()));

        // Title can be popped from stack and set as the window title.
        term.pop_title();
        assert_eq!(term.title, Some("Test".into()));
        assert!(term.title_stack.is_empty());

        // Title stack doesn't grow infinitely.
        for _ in 0..4097 {
            term.push_title();
        }
        assert_eq!(term.title_stack.len(), 4096);

        // Title and title stack reset when terminal state is reset.
        term.push_title();
        term.reset_state();
        assert_eq!(term.title, None);
        assert!(term.title_stack.is_empty());

        // Title stack pops back to default.
        term.title = None;
        term.push_title();
        term.set_title(Some("Test".into()));
        term.pop_title();
        assert_eq!(term.title, None);

        // Title can be reset to default.
        term.title = Some("Test".into());
        term.set_title(None);
        assert_eq!(term.title, None);
    }
}

#[cfg(all(test, feature = "bench"))]
mod benches {
    extern crate serde_json as json;
    extern crate test;

    use std::fs;
    use std::mem;

    use crate::config::MockConfig;
    use crate::event::{Event, EventListener};
    use crate::grid::Grid;

    use super::cell::Cell;
    use super::{SizeInfo, Term};

    struct Mock;
    impl EventListener for Mock {
        fn send_event(&self, _event: Event) {}
    }

    /// Benchmark for the renderable cells iterator.
    ///
    /// The renderable cells iterator yields cells that require work to be
    /// displayed (that is, not an empty background cell). This benchmark
    /// measures how long it takes to process the whole iterator.
    ///
    /// When this benchmark was first added, it averaged ~78usec on my macbook
    /// pro. The total render time for this grid is anywhere between ~1500 and
    /// ~2000usec (measured imprecisely with the visual meter).
    #[bench]
    fn render_iter(b: &mut test::Bencher) {
        // Need some realistic grid state; using one of the ref files.
        let serialized_grid = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/ref/vim_large_window_scroll/grid.json"
        ))
        .unwrap();
        let serialized_size = fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/ref/vim_large_window_scroll/size.json"
        ))
        .unwrap();

        let mut grid: Grid<Cell> = json::from_str(&serialized_grid).unwrap();
        let size: SizeInfo = json::from_str(&serialized_size).unwrap();

        let config = MockConfig::default();

        let mut terminal = Term::new(&config, &size, Mock);
        mem::swap(&mut terminal.grid, &mut grid);

        b.iter(|| {
            let iter = terminal.renderable_cells(&config);
            for cell in iter {
                test::black_box(cell);
            }
        })
    }
}
