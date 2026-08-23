//! Dates, and the calendar that picks one.
//!
//! ## Why a date type lives here
//!
//! SphereKit has no date dependency and is not about to grow one: `chrono` and
//! `time` are both large, both carry a time zone database, and a calendar
//! widget needs none of it. What it needs is the civil calendar — which day of
//! the week a date falls on, how many days a month has, what the month before
//! this one is — and that is a hundred lines of arithmetic with no ambiguity in
//! it. [`Date`] is exactly that and nothing more: no clock, no zone, no
//! instant.
//!
//! The conversion is Howard Hinnant's `days_from_civil`, which is exact for the
//! whole proleptic Gregorian range rather than only for years after 1900, and
//! is the algorithm the C++ standard's `<chrono>` calendar is specified in
//! terms of.
//!
//! ## The state model, unchanged
//!
//! A [`Calendar`] owns neither the selection nor the month on display. It takes
//! both and reports what the gesture asked for:
//!
//! ```ignore
//! calendar(self.month.get(), self.selected.get())
//!     .on_select(move |d| selected.set(Some(d)))
//!     .on_month(move |m| month.set(m))
//! ```
//!
//! That looks like more wiring than a widget that remembered its own month,
//! until the application needs to open the picker on the month of an existing
//! booking — at which point the widget that remembered would have to be told
//! anyway, through an API that does not exist.

use crate::element::{Element, EventContext, PaintContext, Styled};
use crate::event::{EventFlow, Key, MouseButton, UiEvent};
use crate::semantics::{Role, Semantics};
use crate::style::{Cursor, FocusRing, PaintStyle};
use spherekit_core::{Color, Corners, ElementId, Point, Px, Rect, RoundedRect, Size, px};
use spherekit_layout::Style;

/// Hovered cell, one-based. Zero means none.
///
/// One-based because scratch starts zeroed, and a zero that meant "the first
/// cell" would light one up before the pointer had ever entered the grid.
const SCRATCH_HOVER: usize = 0;
/// Which header arrow is hovered: `0` none, `1` previous, `2` next.
const SCRATCH_ARROW: usize = 1;
/// The keyboard cursor, as days from the epoch. Zero means unset.
const SCRATCH_CURSOR: usize = 2;

/// Height of the month header strip, in logical pixels.
const HEADER_H: f32 = 32.0;
/// Height of the weekday name strip.
const WEEKDAY_H: f32 = 22.0;
/// How many week rows a month grid always draws.
///
/// Six, always — never five, and never a count that depends on the month. A
/// grid that changed height between March and April would move every control
/// under it twice a year, which is a worse cost than one blank row.
const WEEK_ROWS: usize = 6;
/// The default side of one day cell.
const CELL: f32 = 34.0;

// ---------------------------------------------------------------------------
// Date
// ---------------------------------------------------------------------------

/// A day of the week.
///
/// Ordered from Monday because ISO-8601 is, and because the alternative — an
/// enum that starts on Sunday — makes "the working week" a wrap-around range.
/// Which day a calendar *starts* on is a separate question, answered by
/// [`Calendar::week_start`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Weekday {
    /// Monday.
    Monday,
    /// Tuesday.
    Tuesday,
    /// Wednesday.
    Wednesday,
    /// Thursday.
    Thursday,
    /// Friday.
    Friday,
    /// Saturday.
    Saturday,
    /// Sunday.
    Sunday,
}

impl Weekday {
    /// Every weekday, Monday first.
    pub const ALL: [Weekday; 7] = [
        Weekday::Monday,
        Weekday::Tuesday,
        Weekday::Wednesday,
        Weekday::Thursday,
        Weekday::Friday,
        Weekday::Saturday,
        Weekday::Sunday,
    ];

    /// The day's position, Monday being zero.
    #[inline]
    pub fn index(self) -> u8 {
        self as u8
    }

    /// Builds from a Monday-based index, wrapping.
    #[inline]
    pub fn from_index(index: i32) -> Self {
        Self::ALL[index.rem_euclid(7) as usize]
    }

    /// The two-letter abbreviation shown in a calendar's header row.
    pub fn short_name(self) -> &'static str {
        match self {
            Weekday::Monday => "Mo",
            Weekday::Tuesday => "Tu",
            Weekday::Wednesday => "We",
            Weekday::Thursday => "Th",
            Weekday::Friday => "Fr",
            Weekday::Saturday => "Sa",
            Weekday::Sunday => "Su",
        }
    }

    /// The full name.
    pub fn name(self) -> &'static str {
        match self {
            Weekday::Monday => "Monday",
            Weekday::Tuesday => "Tuesday",
            Weekday::Wednesday => "Wednesday",
            Weekday::Thursday => "Thursday",
            Weekday::Friday => "Friday",
            Weekday::Saturday => "Saturday",
            Weekday::Sunday => "Sunday",
        }
    }

    /// True for Saturday and Sunday.
    #[inline]
    pub fn is_weekend(self) -> bool {
        matches!(self, Weekday::Saturday | Weekday::Sunday)
    }
}

/// A civil date in the proleptic Gregorian calendar.
///
/// No time, no zone, no clock. Two dates compare and sort in calendar order,
/// which is what the derived `Ord` gives us for free because the fields are
/// declared most-significant first.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: i32,
    month: u8,
    day: u8,
}

/// The month names, January first.
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

impl Date {
    /// Builds a date, or `None` if it does not exist.
    ///
    /// The 30th of February is rejected rather than rolled into March: a
    /// constructor that silently moves a date is how an off-by-one in a booking
    /// system survives review.
    pub fn new(year: i32, month: u32, day: u32) -> Option<Self> {
        if !(1..=12).contains(&month) {
            return None;
        }
        if day < 1 || day > Self::days_in_month(year, month) {
            return None;
        }
        Some(Self { year, month: month as u8, day: day as u8 })
    }

    /// Builds a date, clamping the day into the month.
    ///
    /// For arithmetic that lands on the 31st of a 30-day month — "the same day
    /// next month" — where clamping is the answer everybody actually wants.
    pub fn clamped(year: i32, month: u32, day: u32) -> Self {
        let month = month.clamp(1, 12);
        let day = day.clamp(1, Self::days_in_month(year, month));
        Self { year, month: month as u8, day: day as u8 }
    }

    /// The year.
    #[inline]
    pub fn year(self) -> i32 {
        self.year
    }

    /// The month, `1..=12`.
    #[inline]
    pub fn month(self) -> u32 {
        self.month as u32
    }

    /// The day of the month, `1..=31`.
    #[inline]
    pub fn day(self) -> u32 {
        self.day as u32
    }

    /// True for a Gregorian leap year.
    #[inline]
    pub fn is_leap_year(year: i32) -> bool {
        (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
    }

    /// How many days a month has.
    pub fn days_in_month(year: i32, month: u32) -> u32 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if Self::is_leap_year(year) => 29,
            2 => 28,
            _ => 0,
        }
    }

    /// The month's full name.
    pub fn month_name(self) -> &'static str {
        MONTH_NAMES[(self.month as usize - 1).min(11)]
    }

    /// The month's three-letter abbreviation.
    pub fn month_short(self) -> &'static str {
        &self.month_name()[..3]
    }

    /// Days since 1970-01-01, negative before it.
    ///
    /// Howard Hinnant's `days_from_civil`: exact across the whole proleptic
    /// Gregorian range, with no table and no branch per month.
    pub fn to_epoch_days(self) -> i64 {
        let (y, m, d) = (self.year as i64, self.month as i64, self.day as i64);
        // March-based year: putting the leap day at the end of the year is what
        // removes February as a special case from everything below.
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// Builds from days since 1970-01-01: the exact inverse of
    /// [`Date::to_epoch_days`].
    pub fn from_epoch_days(days: i64) -> Self {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = mp + if mp < 10 { 3 } else { -9 };
        Self { year: (y + i64::from(m <= 2)) as i32, month: m as u8, day: d as u8 }
    }

    /// Today, in UTC.
    ///
    /// UTC rather than local time, and named so. Resolving a local date needs a
    /// time zone database, which is exactly the dependency this module exists
    /// to avoid; an application that has one should pass its own answer to
    /// [`Calendar::today`] instead.
    pub fn today_utc() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Self::from_epoch_days(secs.div_euclid(86_400))
    }

    /// Which day of the week this date falls on.
    pub fn weekday(self) -> Weekday {
        // 1970-01-01 was a Thursday, which is index 3 counting from Monday.
        Weekday::from_index((self.to_epoch_days() + 3).rem_euclid(7) as i32)
    }

    /// The first of this date's month.
    #[inline]
    pub fn first_of_month(self) -> Self {
        Self { day: 1, ..self }
    }

    /// The last day of this date's month.
    #[inline]
    pub fn last_of_month(self) -> Self {
        Self { day: Self::days_in_month(self.year, self.month as u32) as u8, ..self }
    }

    /// Moves by whole days.
    #[inline]
    pub fn add_days(self, days: i32) -> Self {
        Self::from_epoch_days(self.to_epoch_days() + days as i64)
    }

    /// Moves by whole months, clamping the day.
    ///
    /// The 31st of January plus one month is the 28th or 29th of February, not
    /// the 3rd of March. Every calendar in the world agrees on this and the
    /// naive implementation gets it wrong.
    pub fn add_months(self, months: i32) -> Self {
        let total = self.year as i64 * 12 + (self.month as i64 - 1) + months as i64;
        let year = total.div_euclid(12) as i32;
        let month = total.rem_euclid(12) as u32 + 1;
        Self::clamped(year, month, self.day as u32)
    }

    /// Moves by whole years, clamping the 29th of February.
    #[inline]
    pub fn add_years(self, years: i32) -> Self {
        Self::clamped(self.year + years, self.month as u32, self.day as u32)
    }

    /// True when both dates name the same month of the same year.
    #[inline]
    pub fn same_month(self, other: Self) -> bool {
        self.year == other.year && self.month == other.month
    }

    /// How many days separate the two, negative when `other` is later.
    #[inline]
    pub fn days_until(self, other: Self) -> i64 {
        other.to_epoch_days() - self.to_epoch_days()
    }

    /// Formats as `YYYY-MM-DD`.
    ///
    /// ISO-8601 rather than a locale format: this is the string a program
    /// stores and compares, and it sorts lexicographically in date order.
    pub fn iso(self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// Formats as `12 March 2026`, for a heading a person reads.
    pub fn long(self) -> String {
        format!("{} {} {}", self.day, self.month_name(), self.year)
    }
}

impl core::fmt::Display for Date {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.iso())
    }
}

// ---------------------------------------------------------------------------
// Calendar
// ---------------------------------------------------------------------------

/// A change callback taking a date.
pub type OnDate = Box<dyn FnMut(Date)>;

/// A month grid: pick a day, page between months.
///
/// One element rather than forty-two, and the reason is measurable: a composed
/// calendar builds six weeks of seven boxes with a label inside each, which is
/// ninety nodes to reconcile and lay out every time a hover changes. This paints
/// the grid directly and hit-tests it with arithmetic, so a hover is one
/// repaint of one node and a month change never touches layout at all.
///
/// The widget owns nothing: `month` says which page is on screen and `selected`
/// says which day is chosen, and both come from the caller.
pub struct Calendar {
    id: Option<ElementId>,
    /// Any date in the month being displayed; its day is ignored.
    month: Date,
    selected: Option<Date>,
    today: Option<Date>,
    range: Option<(Date, Date)>,
    min: Option<Date>,
    max: Option<Date>,
    week_start: Weekday,
    show_adjacent: bool,
    disabled: bool,
    cell: Px,
    style: Style,
    paint: PaintStyle,
    on_select: Option<OnDate>,
    on_month: Option<OnDate>,
}

/// Creates a [`Calendar`] showing `month`, with `selected` highlighted.
pub fn calendar(month: Date, selected: Option<Date>) -> Calendar {
    Calendar {
        id: None,
        month: month.first_of_month(),
        selected,
        today: Some(Date::today_utc()),
        range: None,
        min: None,
        max: None,
        week_start: Weekday::Monday,
        show_adjacent: true,
        disabled: false,
        cell: px(CELL),
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
        on_select: None,
        on_month: None,
    }
}

impl Calendar {
    /// Gives the calendar a stable identity, which it needs to keep its
    /// keyboard cursor and its focus across rebuilds.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Marks a date as today, which is drawn with a ring rather than a fill.
    ///
    /// Defaults to [`Date::today_utc`]. An application with a time zone should
    /// pass its own answer; `None` removes the marker.
    pub fn today(mut self, today: Option<Date>) -> Self {
        self.today = today;
        self
    }

    /// Highlights a closed span of days, for a range selection in progress.
    pub fn range(mut self, range: Option<(Date, Date)>) -> Self {
        self.range = range.map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
        self
    }

    /// Refuses dates before this one.
    pub fn min(mut self, min: Date) -> Self {
        self.min = Some(min);
        self
    }

    /// Refuses dates after this one.
    pub fn max(mut self, max: Date) -> Self {
        self.max = Some(max);
        self
    }

    /// Sets which day the week starts on. Defaults to Monday.
    pub fn week_start(mut self, day: Weekday) -> Self {
        self.week_start = day;
        self
    }

    /// Shows the days either side of the month, muted. On by default.
    ///
    /// Turning it off leaves the leading and trailing cells blank, which some
    /// designs prefer; the cells stay in place either way, because the grid's
    /// height never changes.
    pub fn show_adjacent(mut self, show: bool) -> Self {
        self.show_adjacent = show;
        self
    }

    /// Marks the whole calendar disabled.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Sets the side of one day cell. Defaults to 34 px.
    pub fn cell_size(mut self, cell: Px) -> Self {
        self.cell = cell;
        self
    }

    /// Runs when a day is chosen.
    pub fn on_select(mut self, f: impl FnMut(Date) + 'static) -> Self {
        self.on_select = Some(Box::new(f));
        self
    }

    /// Runs when the displayed month changes, with the first of the new month.
    ///
    /// Fired by the header arrows, by PageUp and PageDown, and by any keyboard
    /// or pointer move that leaves the month on display — a click on a trailing
    /// grey day selects it *and* pages, because that is what the user meant.
    pub fn on_month(mut self, f: impl FnMut(Date) + 'static) -> Self {
        self.on_month = Some(Box::new(f));
        self
    }

    /// True when a date is inside the allowed range.
    fn allows(&self, date: Date) -> bool {
        self.min.is_none_or(|min| date >= min) && self.max.is_none_or(|max| date <= max)
    }

    /// The date in the top-left cell of the grid.
    ///
    /// The last `week_start` on or before the first of the month, so the first
    /// row is always a full week.
    fn grid_origin(&self) -> Date {
        let first = self.month.first_of_month();
        let lead = (first.weekday().index() as i32 - self.week_start.index() as i32).rem_euclid(7);
        first.add_days(-lead)
    }

    /// The date in cell `index`, counting left to right and top to bottom.
    fn date_at(&self, index: usize) -> Date {
        self.grid_origin().add_days(index as i32)
    }

    /// The keyboard cursor: where the arrow keys act from.
    ///
    /// The scratch slot first, then the selection, then the first of the month.
    /// Stored as days from the epoch, which is exact in an `f32` for every year
    /// between 24 000 BC and 47 000 AD — comfortably more calendar than anyone
    /// is going to page through.
    fn keyboard_cursor(&self, scratch: [f32; 4]) -> Date {
        let stored = scratch[SCRATCH_CURSOR];
        if stored != 0.0 {
            let date = Date::from_epoch_days(stored as i64);
            if date.same_month(self.month) {
                return date;
            }
        }
        match self.selected {
            Some(d) if d.same_month(self.month) => d,
            _ => self.month.first_of_month(),
        }
    }

    /// The three strips the box divides into: header, weekday names, grid.
    fn regions(&self, b: Rect<Px>) -> (Rect<Px>, Rect<Px>, Rect<Px>) {
        let header = Rect::new(b.origin, Size::new(b.width(), px(HEADER_H)));
        let weekdays = Rect::new(
            Point::new(b.min_x(), b.min_y() + px(HEADER_H)),
            Size::new(b.width(), px(WEEKDAY_H)),
        );
        let grid = Rect::from_corners(
            Point::new(b.min_x(), b.min_y() + px(HEADER_H + WEEKDAY_H)),
            Point::new(b.max_x(), b.max_y()),
        );
        (header, weekdays, grid)
    }

    /// The two header arrows, previous first.
    fn arrows(&self, header: Rect<Px>) -> (Rect<Px>, Rect<Px>) {
        let side = px(HEADER_H - 4.0);
        let y = header.min_y() + px(2.0);
        (
            Rect::new(Point::new(header.min_x(), y), Size::new(side, side)),
            Rect::new(Point::new(header.max_x() - side, y), Size::new(side, side)),
        )
    }

    /// The box of one grid cell.
    fn cell_rect(&self, grid: Rect<Px>, index: usize) -> Rect<Px> {
        let w = grid.width() / 7.0;
        let h = grid.height() / WEEK_ROWS as f32;
        let (col, row) = (index % 7, index / 7);
        Rect::new(
            Point::new(grid.min_x() + w * col as f32, grid.min_y() + h * row as f32),
            Size::new(w, h),
        )
    }

    /// Which cell a point is in, if any.
    fn cell_at(&self, grid: Rect<Px>, at: Point<Px>) -> Option<usize> {
        if !grid.contains(at) || grid.is_empty() {
            return None;
        }
        let col = ((at.x - grid.min_x()) / (grid.width() / 7.0)) as usize;
        let row = ((at.y - grid.min_y()) / (grid.height() / WEEK_ROWS as f32)) as usize;
        let index = row.min(WEEK_ROWS - 1) * 7 + col.min(6);
        Some(index)
    }

    /// Reports a selection, and pages the month when the day is not in it.
    fn choose(&mut self, date: Date) {
        if !self.allows(date) {
            return;
        }
        if !date.same_month(self.month)
            && let Some(f) = self.on_month.as_mut()
        {
            f(date.first_of_month());
        }
        if let Some(f) = self.on_select.as_mut() {
            f(date);
        }
    }

    /// Reports a page turn.
    fn page(&mut self, months: i32) {
        if let Some(f) = self.on_month.as_mut() {
            f(self.month.add_months(months).first_of_month());
        }
    }

    /// Moves the keyboard cursor, paging if it leaves the month.
    ///
    /// Arrow keys are how a user walks off the end of a month, so the page turn
    /// has to be part of the same gesture — a cursor that stopped at the 31st
    /// would make the first of the next month unreachable from the keyboard.
    fn move_cursor(&mut self, cx: &mut EventContext<'_>, to: Date) {
        cx.scratch[SCRATCH_CURSOR] = to.to_epoch_days() as f32;
        if !to.same_month(self.month) {
            let here = self.month.year() * 12 + self.month.month() as i32;
            let there = to.year() * 12 + to.month() as i32;
            self.page(there - here);
        }
        cx.notify();
    }
}

impl Styled for Calendar {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }

    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for Calendar {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        let mut style = self.style.clone();
        if matches!(style.size.width, spherekit_core::Length::Auto) {
            style.size.width = spherekit_core::Length::Px(self.cell * 7.0);
        }
        if matches!(style.size.height, spherekit_core::Length::Auto) {
            style.size.height =
                spherekit_core::Length::Px(self.cell * WEEK_ROWS as f32 + px(HEADER_H + WEEKDAY_H));
        }
        style
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        cx.keep_interactive();
        let b = cx.bounds;
        if b.is_empty() {
            return;
        }
        let c = cx.theme.colors;
        let t = cx.theme.typography;
        self.paint.paint_box(cx.canvas, b, cx.state);

        let (header, weekdays, grid) = self.regions(b);
        let (prev, next) = self.arrows(header);
        let hovered_arrow = cx.scratch[SCRATCH_ARROW] as i32 - 1;
        let hovered_cell = cx.scratch[SCRATCH_HOVER] as i32 - 1;
        let cursor = self.keyboard_cursor(cx.scratch);
        let dim = if self.disabled { 0.45 } else { 1.0 };

        // --- header ---------------------------------------------------------
        for (i, rect) in [prev, next].into_iter().enumerate() {
            if hovered_arrow == i as i32 && !self.disabled {
                cx.canvas.fill_rounded_rect(RoundedRect::uniform(rect, cx.theme.radii.sm), c.hover);
            }
            // Drawn rather than shaped: an icon font is a dependency the widget
            // would otherwise impose on every application that used it.
            paint_chevron(cx, rect, i == 0, c.text_muted.scale_alpha(dim));
        }
        let title = format!("{} {}", self.month.month_name(), self.month.year());
        draw_centered(
            cx,
            &title,
            t.md,
            Some(t.strong),
            Rect::from_corners(
                Point::new(prev.max_x(), header.min_y()),
                Point::new(next.min_x(), header.max_y()),
            ),
            c.text.scale_alpha(dim),
        );

        // --- weekday names --------------------------------------------------
        for col in 0..7 {
            let day = Weekday::from_index(self.week_start.index() as i32 + col);
            let cell = Rect::new(
                Point::new(
                    weekdays.min_x() + weekdays.width() / 7.0 * col as f32,
                    weekdays.min_y(),
                ),
                Size::new(weekdays.width() / 7.0, weekdays.height()),
            );
            draw_centered(
                cx,
                day.short_name(),
                t.xs,
                Some(t.strong),
                cell,
                c.text_muted.scale_alpha(dim),
            );
        }

        // --- the grid -------------------------------------------------------
        for index in 0..WEEK_ROWS * 7 {
            let date = self.date_at(index);
            let outside = !date.same_month(self.month);
            if outside && !self.show_adjacent {
                continue;
            }
            let cell = self.cell_rect(grid, index);
            // A square inside the cell, so the selected pill is round rather
            // than a wide lozenge in a grid whose columns are wider than tall.
            let side = Px(cell.width().get().min(cell.height().get()) - 4.0).max(px(8.0));
            let box_ = Rect::new(
                Point::new(cell.center().x - side * 0.5, cell.center().y - side * 0.5),
                Size::new(side, side),
            );
            let radius = cx.theme.radii.md;
            let allowed = self.allows(date) && !self.disabled;
            let selected = self.selected == Some(date);

            // A range band spans the whole cell rather than the inner square,
            // so consecutive days join into one continuous bar.
            if let Some((from, to)) = self.range
                && date >= from
                && date <= to
            {
                let ends = Corners {
                    top_left: if date == from { radius } else { Px::ZERO },
                    bottom_left: if date == from { radius } else { Px::ZERO },
                    top_right: if date == to { radius } else { Px::ZERO },
                    bottom_right: if date == to { radius } else { Px::ZERO },
                };
                let band = Rect::new(
                    Point::new(cell.min_x(), box_.min_y()),
                    Size::new(cell.width(), box_.height()),
                );
                cx.canvas
                    .fill_rounded_rect(RoundedRect::new(band, ends), c.accent.with_alpha(0.18));
            }

            if selected {
                cx.canvas.fill_rounded_rect(
                    RoundedRect::uniform(box_, radius),
                    c.accent.scale_alpha(dim),
                );
            } else if hovered_cell == index as i32 && allowed {
                cx.canvas.fill_rounded_rect(RoundedRect::uniform(box_, radius), c.hover);
            }
            if self.today == Some(date) && !selected {
                cx.canvas.stroke_rounded_rect(
                    RoundedRect::uniform(box_, radius),
                    c.accent.scale_alpha(dim),
                    px(1.0),
                );
            }
            if cx.state.focused && date == cursor {
                let ring = FocusRing { color: c.focus, ..FocusRing::default() };
                let style = PaintStyle {
                    focus_ring: Some(ring),
                    corner_radii: Corners::all(radius),
                    ..Default::default()
                };
                style.paint_box(cx.canvas, box_, cx.state);
            }

            // Three weights, not five. A day that belongs to another month and
            // a day that is a weekend are both *there but not the answer*, and
            // giving them separate greys would invent a distinction the reader
            // then has to work out.
            let color = if !allowed {
                c.text_muted.scale_alpha(0.45)
            } else if selected {
                c.text_on_accent
            } else if outside || date.weekday().is_weekend() {
                c.text_muted.scale_alpha(dim)
            } else {
                c.text.scale_alpha(dim)
            };
            draw_centered(cx, &date.day().to_string(), t.sm, None, box_, color);
        }
    }

    fn handle_event(&mut self, cx: &mut EventContext<'_>) -> EventFlow {
        if self.disabled {
            return EventFlow::Continue;
        }
        let (header, _, grid) = self.regions(cx.bounds);
        let (prev, next) = self.arrows(header);

        match cx.event {
            UiEvent::MouseMove(e) => {
                let arrow = if prev.contains(e.position) {
                    1
                } else if next.contains(e.position) {
                    2
                } else {
                    0
                };
                let cell = match self.cell_at(grid, e.position) {
                    Some(i) if self.allows(self.date_at(i)) => i as i32 + 1,
                    _ => 0,
                };
                if cx.scratch[SCRATCH_ARROW] as i32 != arrow
                    || cx.scratch[SCRATCH_HOVER] as i32 != cell
                {
                    cx.scratch[SCRATCH_ARROW] = arrow as f32;
                    cx.scratch[SCRATCH_HOVER] = cell as f32;
                    cx.notify();
                }
                if arrow > 0 || cell > 0 {
                    cx.set_cursor(Cursor::Pointer);
                }
                EventFlow::Continue
            }
            UiEvent::MouseLeave(_) => {
                // Cleared on the way out, or the last cell the pointer touched
                // stays lit while the pointer is somewhere else entirely.
                if cx.scratch[SCRATCH_HOVER] != 0.0 || cx.scratch[SCRATCH_ARROW] != 0.0 {
                    cx.scratch[SCRATCH_HOVER] = 0.0;
                    cx.scratch[SCRATCH_ARROW] = 0.0;
                    cx.notify();
                }
                EventFlow::Continue
            }
            UiEvent::MouseDown(e) if e.button == MouseButton::Primary => {
                cx.focus();
                if prev.contains(e.position) {
                    self.page(-1);
                    cx.notify();
                    return EventFlow::Stop;
                }
                if next.contains(e.position) {
                    self.page(1);
                    cx.notify();
                    return EventFlow::Stop;
                }
                if let Some(index) = self.cell_at(grid, e.position) {
                    let date = self.date_at(index);
                    cx.scratch[SCRATCH_CURSOR] = date.to_epoch_days() as f32;
                    self.choose(date);
                    cx.notify();
                    return EventFlow::Stop;
                }
                EventFlow::Continue
            }
            UiEvent::Scroll(e) => {
                let lines = match e.delta {
                    crate::event::ScrollDelta::Lines(d) => d.height,
                    crate::event::ScrollDelta::Pixels(d) => d.height.get() / 40.0,
                };
                if lines.abs() < 0.5 {
                    return EventFlow::Continue;
                }
                // Up pages backward, which is the direction the months are
                // going as the grid moves down under the pointer.
                self.page(if lines > 0.0 { -1 } else { 1 });
                cx.notify();
                EventFlow::Stop
            }
            UiEvent::Key(k) if k.state.is_pressed() => {
                let cursor = self.keyboard_cursor(*cx.scratch);
                let step = match k.key {
                    Key::Left => Some(-1),
                    Key::Right => Some(1),
                    Key::Up => Some(-7),
                    Key::Down => Some(7),
                    _ => None,
                };
                if let Some(days) = step {
                    let to = cursor.add_days(days);
                    self.move_cursor(cx, to);
                    return EventFlow::Stop;
                }
                match k.key {
                    Key::PageUp => {
                        let months = if k.modifiers.shift { -12 } else { -1 };
                        cx.scratch[SCRATCH_CURSOR] =
                            cursor.add_months(months).to_epoch_days() as f32;
                        self.page(months);
                        cx.notify();
                        EventFlow::Stop
                    }
                    Key::PageDown => {
                        let months = if k.modifiers.shift { 12 } else { 1 };
                        cx.scratch[SCRATCH_CURSOR] =
                            cursor.add_months(months).to_epoch_days() as f32;
                        self.page(months);
                        cx.notify();
                        EventFlow::Stop
                    }
                    Key::Home => {
                        self.move_cursor(cx, self.month.first_of_month());
                        EventFlow::Stop
                    }
                    Key::End => {
                        self.move_cursor(cx, self.month.last_of_month());
                        EventFlow::Stop
                    }
                    Key::Enter | Key::Space => {
                        self.choose(cursor);
                        cx.notify();
                        EventFlow::Stop
                    }
                    _ => EventFlow::Continue,
                }
            }
            _ => EventFlow::Continue,
        }
    }

    fn focusable(&self) -> bool {
        !self.disabled
    }

    fn semantics(&self) -> Option<Semantics> {
        let name = format!("{} {}", self.month.month_name(), self.month.year());
        Some(
            Semantics::new(Role::Group, name)
                .value_text(self.selected.map(|d| d.long()).unwrap_or_else(|| "None".into()))
                .disabled(self.disabled)
                .with_implied_actions(),
        )
    }
}

/// Draws one line of text centred in a box.
///
/// A calendar paints forty-two numbers, two headings and seven names; doing it
/// through child elements would put ninety nodes in the tree for text that
/// never moves relative to the cell it sits in. Shared with the widgets that
/// paint their own labels for the same reason.
pub(crate) fn draw_centered(
    cx: &mut PaintContext<'_, '_>,
    text: &str,
    size: Px,
    weight: Option<spherekit_text::FontWeight>,
    into: Rect<Px>,
    color: Color,
) {
    if text.is_empty() || into.is_empty() {
        return;
    }
    let style = spherekit_text::TextStyle {
        font_size: size,
        font: spherekit_text::FontRequest {
            weight: weight.unwrap_or(cx.theme.typography.weight),
            ..Default::default()
        },
        wrap: spherekit_text::WrapMode::None,
        ..Default::default()
    };
    let layout = cx.text.layout(text, &style, None);
    let origin = Point::new(
        into.min_x() + (into.width() - layout.size.width) * 0.5,
        into.min_y() + (into.height() - layout.size.height) * 0.5,
    );
    crate::text::draw_layout(
        cx.canvas,
        &layout,
        origin,
        color,
        spherekit_render::TextRasterMode::Auto,
        (Px::ZERO, Color::TRANSPARENT),
        spherekit_render::coverage_contrast_for(color, cx.theme.colors.surface),
    );
}

/// Draws a left or right chevron centred in a box.
fn paint_chevron(cx: &mut PaintContext<'_, '_>, into: Rect<Px>, left: bool, color: Color) {
    let centre = into.center();
    let reach = Px(into.width().get() * 0.16);
    let rise = Px(into.height().get() * 0.20);
    let dir = if left { -1.0 } else { 1.0 };
    let tip = Point::new(centre.x + reach * dir, centre.y);
    cx.canvas.draw_line(Point::new(centre.x - reach * dir, centre.y - rise), tip, color, px(1.5));
    cx.canvas.draw_line(tip, Point::new(centre.x - reach * dir, centre.y + rise), color, px(1.5));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mounts a calendar of a known size, so a cell's centre is arithmetic.
    fn mount(cal: Calendar) -> crate::tree::UiTree {
        use crate::element::{IntoElement, ParentElement, Styled};
        let mut tree = crate::tree::UiTree::new();
        tree.build(
            crate::element::div()
                .w(px(GRID_W))
                .h(px(GRID_H))
                .child(cal.into_element())
                .into_element(),
        );
        tree.compute_layout(spherekit_core::size(px(400.0), px(400.0))).unwrap();
        tree
    }

    /// A test calendar's width: seven 34 px columns.
    const GRID_W: f32 = CELL * 7.0;
    /// And its height: the header, the names, and six rows.
    const GRID_H: f32 = HEADER_H + WEEKDAY_H + CELL * WEEK_ROWS as f32;

    /// The centre of one grid cell in the mounted calendar.
    fn cell_centre(index: usize) -> (f32, f32) {
        let (col, row) = (index % 7, index / 7);
        (CELL * (col as f32 + 0.5), HEADER_H + WEEKDAY_H + CELL * (row as f32 + 0.5))
    }

    fn press_at(x: f32, y: f32) -> crate::event::UiEvent {
        crate::event::UiEvent::MouseDown(crate::event::MouseButtonEvent {
            position: Point::new(px(x), px(y)),
            button: MouseButton::Primary,
            state: crate::event::ElementState::Pressed,
            click_count: 1,
            modifiers: crate::event::Modifiers::NONE,
        })
    }

    #[test]
    fn clicking_a_cell_selects_the_day_it_shows() {
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        // March 2026 begins on a Sunday, so with a Monday week start the first
        // row is six trailing days of February and then the 1st.
        let month = Date::new(2026, 3, 1).unwrap();
        let picked = Rc::new(StdCell::new(None));
        let p = Rc::clone(&picked);
        let mut tree = mount(calendar(month, None).id("cal").on_select(move |d| p.set(Some(d))));
        // Cell 7 is the second row's first column: the 2nd of March.
        let (x, y) = cell_centre(7);
        tree.dispatch(&press_at(x, y));
        assert_eq!(picked.get(), Date::new(2026, 3, 2));
    }

    #[test]
    fn clicking_a_trailing_day_pages_as_well_as_selects() {
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        let month = Date::new(2026, 3, 1).unwrap();
        let picked = Rc::new(StdCell::new(None));
        let paged = Rc::new(StdCell::new(None));
        let (p, m) = (Rc::clone(&picked), Rc::clone(&paged));
        let mut tree = mount(
            calendar(month, None)
                .id("cal")
                .on_select(move |d| p.set(Some(d)))
                .on_month(move |d| m.set(Some(d))),
        );
        // Cell 0 is the leading Monday: the 23rd of February.
        let (x, y) = cell_centre(0);
        tree.dispatch(&press_at(x, y));
        assert_eq!(picked.get(), Date::new(2026, 2, 23));
        assert_eq!(paged.get(), Date::new(2026, 2, 1), "the view did not follow the click");
    }

    #[test]
    fn the_header_arrows_page_without_selecting() {
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        let month = Date::new(2026, 3, 1).unwrap();
        let picked = Rc::new(StdCell::new(None));
        let paged = Rc::new(StdCell::new(None));
        let (p, m) = (Rc::clone(&picked), Rc::clone(&paged));
        let mut tree = mount(
            calendar(month, None)
                .id("cal")
                .on_select(move |d| p.set(Some(d)))
                .on_month(move |d| m.set(Some(d))),
        );
        tree.dispatch(&press_at(HEADER_H * 0.5, HEADER_H * 0.5));
        assert_eq!(paged.get(), Date::new(2026, 2, 1));
        assert_eq!(picked.get(), None, "paging is not a selection");

        tree.dispatch(&press_at(GRID_W - HEADER_H * 0.5, HEADER_H * 0.5));
        assert_eq!(paged.get(), Date::new(2026, 4, 1));
    }

    #[test]
    fn a_day_outside_the_range_refuses_the_click() {
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        let month = Date::new(2026, 3, 1).unwrap();
        let picked = Rc::new(StdCell::new(None));
        let p = Rc::clone(&picked);
        let mut tree = mount(
            calendar(month, None)
                .id("cal")
                .min(Date::new(2026, 3, 10).unwrap())
                .on_select(move |d| p.set(Some(d))),
        );
        let (x, y) = cell_centre(7);
        tree.dispatch(&press_at(x, y));
        assert_eq!(picked.get(), None);
    }

    #[test]
    fn epoch_days_round_trip() {
        for days in [-100_000, -1, 0, 1, 20_000, 100_000] {
            let date = Date::from_epoch_days(days);
            assert_eq!(date.to_epoch_days(), days, "{date} did not round-trip");
        }
    }

    #[test]
    fn the_epoch_is_a_thursday() {
        assert_eq!(Date::from_epoch_days(0), Date::new(1970, 1, 1).unwrap());
        assert_eq!(Date::new(1970, 1, 1).unwrap().weekday(), Weekday::Thursday);
    }

    #[test]
    fn known_weekdays() {
        // A leap day, a century that is not a leap year, and a recent Monday.
        assert_eq!(Date::new(2000, 2, 29).unwrap().weekday(), Weekday::Tuesday);
        assert_eq!(Date::new(1900, 3, 1).unwrap().weekday(), Weekday::Thursday);
        assert_eq!(Date::new(2026, 8, 24).unwrap().weekday(), Weekday::Monday);
    }

    #[test]
    fn leap_years_follow_the_gregorian_rule() {
        assert!(Date::is_leap_year(2024));
        assert!(!Date::is_leap_year(1900));
        assert!(Date::is_leap_year(2000));
        assert_eq!(Date::days_in_month(2024, 2), 29);
        assert_eq!(Date::days_in_month(2025, 2), 28);
    }

    #[test]
    fn impossible_dates_are_refused_not_rolled() {
        assert!(Date::new(2025, 2, 29).is_none());
        assert!(Date::new(2025, 13, 1).is_none());
        assert!(Date::new(2025, 4, 31).is_none());
        assert_eq!(Date::clamped(2025, 2, 31), Date::new(2025, 2, 28).unwrap());
    }

    #[test]
    fn adding_a_month_clamps_the_day() {
        let jan31 = Date::new(2025, 1, 31).unwrap();
        assert_eq!(jan31.add_months(1), Date::new(2025, 2, 28).unwrap());
        assert_eq!(jan31.add_months(13), Date::new(2026, 2, 28).unwrap());
        assert_eq!(jan31.add_months(-1), Date::new(2024, 12, 31).unwrap());
    }

    #[test]
    fn adding_days_crosses_years() {
        let new_years_eve = Date::new(2025, 12, 31).unwrap();
        assert_eq!(new_years_eve.add_days(1), Date::new(2026, 1, 1).unwrap());
        assert_eq!(new_years_eve.add_days(-365), Date::new(2024, 12, 31).unwrap());
    }

    #[test]
    fn the_grid_always_starts_on_the_week_start() {
        for month in 1..=12 {
            for start in Weekday::ALL {
                let cal = calendar(Date::new(2026, month, 1).unwrap(), None).week_start(start);
                let origin = cal.grid_origin();
                assert_eq!(origin.weekday(), start, "{month} started on the wrong day");
                // And it must not skip the first: six rows of seven have to
                // cover every day of every month, which is why the lead is a
                // backward step rather than a forward one.
                assert!(origin <= cal.month.first_of_month());
                assert!(cal.date_at(WEEK_ROWS * 7 - 1) >= cal.month.last_of_month());
            }
        }
    }

    #[test]
    fn iso_sorts_in_date_order() {
        let mut dates = [
            Date::new(2026, 1, 2).unwrap(),
            Date::new(2025, 12, 31).unwrap(),
            Date::new(2026, 1, 10).unwrap(),
        ];
        dates.sort();
        let iso: Vec<String> = dates.iter().map(|d| d.iso()).collect();
        let mut sorted = iso.clone();
        sorted.sort();
        assert_eq!(iso, sorted);
    }

    #[test]
    fn range_bounds_reject_outside_dates() {
        let cal = calendar(Date::new(2026, 3, 1).unwrap(), None)
            .min(Date::new(2026, 3, 5).unwrap())
            .max(Date::new(2026, 3, 20).unwrap());
        assert!(!cal.allows(Date::new(2026, 3, 4).unwrap()));
        assert!(cal.allows(Date::new(2026, 3, 5).unwrap()));
        assert!(cal.allows(Date::new(2026, 3, 20).unwrap()));
        assert!(!cal.allows(Date::new(2026, 3, 21).unwrap()));
    }
}
