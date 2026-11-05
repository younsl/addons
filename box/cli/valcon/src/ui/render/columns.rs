//! Width-aware column layout: columns never truncate their header. When the panel is too narrow,
//! the lowest-priority columns are dropped, and spare width goes to the fill columns.

use ratatui::layout::Constraint;
use ratatui::widgets::{Cell, Row};

pub const SPACING: u16 = 2;

#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub title: &'static str,
    width: u16,
    fill: bool,
    priority: u8,
}

impl Column {
    pub const fn fixed(title: &'static str, width: u16, priority: u8) -> Self {
        Self {
            title,
            width,
            fill: false,
            priority,
        }
    }

    pub const fn fill(title: &'static str, min: u16, priority: u8) -> Self {
        Self {
            title,
            width: min,
            fill: true,
            priority,
        }
    }

    fn min_width(&self) -> u16 {
        let title = u16::try_from(self.title.chars().count()).unwrap_or(u16::MAX);
        self.width.max(title)
    }
}

/// Kept column indices with their widths, in display order.
pub struct Fitted {
    columns: Vec<(usize, u16)>,
    titles: Vec<&'static str>,
}

impl Fitted {
    pub fn constraints(&self) -> Vec<Constraint> {
        self.columns
            .iter()
            .map(|(_, w)| Constraint::Length(*w))
            .collect()
    }

    pub fn header(&self) -> Row<'static> {
        Row::new(self.columns.iter().map(|(i, _)| self.titles[*i]))
    }

    /// Picks the kept cells out of a full row.
    pub fn row<'a>(&self, cells: Vec<Cell<'a>>) -> Row<'a> {
        Row::new(self.pick(cells))
    }

    fn pick<T>(&self, items: Vec<T>) -> Vec<T> {
        let mut items: Vec<Option<T>> = items.into_iter().map(Some).collect();
        self.columns
            .iter()
            .filter_map(|(i, _)| items.get_mut(*i).and_then(Option::take))
            .collect()
    }

    #[cfg(test)]
    fn kept(&self) -> Vec<&'static str> {
        self.columns.iter().map(|(i, _)| self.titles[*i]).collect()
    }
}

pub fn fit(columns: &[Column], available: u16) -> Fitted {
    let mut keep: Vec<usize> = (0..columns.len()).collect();
    let need = |keep: &[usize]| -> u16 {
        let widths: u16 = keep.iter().map(|i| columns[*i].min_width()).sum();
        let gaps = u16::try_from(keep.len().saturating_sub(1)).unwrap_or(u16::MAX);
        widths.saturating_add(gaps.saturating_mul(SPACING))
    };
    while keep.len() > 1 && need(&keep) > available {
        let drop = keep
            .iter()
            .enumerate()
            .min_by_key(|(pos, i)| (columns[**i].priority, std::cmp::Reverse(*pos)))
            .map(|(pos, _)| pos);
        if let Some(pos) = drop {
            keep.remove(pos);
        }
    }

    let fills = keep.iter().filter(|i| columns[**i].fill).count();
    let slack = available.saturating_sub(need(&keep));
    let n = u16::try_from(fills).unwrap_or(u16::MAX);
    let share = slack.checked_div(n).unwrap_or(0);
    let mut remainder = slack.checked_rem(n).unwrap_or(0);

    let fitted = keep
        .iter()
        .map(|i| {
            let mut w = columns[*i].min_width();
            if columns[*i].fill {
                w += share + u16::from(remainder > 0);
                remainder = remainder.saturating_sub(1);
            }
            (*i, w)
        })
        .collect();
    Fitted {
        columns: fitted,
        titles: columns.iter().map(|c| c.title).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: [Column; 5] = [
        Column::fill("NAME", 10, 10),
        Column::fixed("STATE", 3, 9),
        Column::fixed("OTHER", 5, 1),
        Column::fixed("LAST OLD", 8, 5),
        Column::fill("DETAIL", 6, 6),
    ];

    fn total(f: &Fitted) -> u16 {
        let w: u16 = f.columns.iter().map(|(_, w)| *w).sum();
        w + SPACING * u16::try_from(f.columns.len() - 1).unwrap()
    }

    #[test]
    fn wide_panel_keeps_everything_and_fills() {
        let f = fit(&COLS, 80);
        assert_eq!(f.kept(), ["NAME", "STATE", "OTHER", "LAST OLD", "DETAIL"]);
        assert_eq!(total(&f), 80);
        assert_eq!(f.columns[1].1, 5, "header wider than the spec width wins");
    }

    #[test]
    fn narrow_panel_drops_low_priority_first() {
        let f = fit(&COLS, 35);
        assert_eq!(f.kept(), ["NAME", "STATE", "LAST OLD", "DETAIL"]);
        assert_eq!(total(&f), 35);
        let f = fit(&COLS, 34);
        assert_eq!(f.kept(), ["NAME", "STATE", "DETAIL"]);
        let f = fit(&COLS, 20);
        assert_eq!(f.kept(), ["NAME", "STATE"]);
        let f = fit(&COLS, 3);
        assert_eq!(f.kept(), ["NAME"]);
    }

    #[test]
    fn rows_follow_kept_columns() {
        let f = fit(&COLS, 20);
        assert_eq!(f.constraints().len(), 2);
        let _header = f.header();
        let _row = f.row(["a", "b", "c", "d", "e"].map(Cell::from).to_vec());
        assert_eq!(f.pick(vec!["a", "b", "c", "d", "e"]), ["a", "b"]);
    }
}
