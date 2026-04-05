#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]
#![allow(clippy::cargo_common_metadata)]
use std::fmt;
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlign {
    Left,
    Right,
    Center,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableColumn {
    pub header: String,
    pub align: TableAlign,
    pub min_width: usize,
}

impl TableColumn {
    #[must_use]
    pub fn new(header: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            align: TableAlign::Left,
            min_width: 0,
        }
    }

    #[must_use]
    pub const fn align(mut self, align: TableAlign) -> Self {
        self.align = align;
        self
    }

    #[must_use]
    pub const fn min_width(mut self, min_width: usize) -> Self {
        self.min_width = min_width;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    columns: Vec<TableColumn>,
    rows: Vec<Vec<String>>,
}

impl Table {
    #[must_use]
    pub const fn new(columns: Vec<TableColumn>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_rows(mut self, rows: Vec<Vec<String>>) -> Self {
        self.rows = rows;
        self
    }

    pub fn push_row(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    #[must_use]
    pub fn columns(&self) -> &[TableColumn] {
        &self.columns
    }

    #[must_use]
    pub fn rows(&self) -> &[Vec<String>] {
        &self.rows
    }
}

/// Write a formatted table to the given writer.
///
/// Each row is written as a single line with columns padded to their computed
/// widths according to the alignment specified in each [`TableColumn`].
///
/// # Errors
///
/// Returns an [`io::Error`] if any write to `out` fails.
pub fn write_table<W>(out: &mut W, table: &Table) -> io::Result<()>
where
    W: io::Write,
{
    write_table_internal(table, |line| {
        out.write_all(line.as_bytes())?;
        out.write_all(b"\n")
    })
}

/// Write a formatted table using the [`fmt::Write`] trait.
///
/// Behaves like [`write_table`] but targets any `fmt::Write` destination
/// (e.g. a `String`).
///
/// # Errors
///
/// Returns [`fmt::Error`] if any write to `out` fails.
pub fn write_table_fmt<W>(out: &mut W, table: &Table) -> fmt::Result
where
    W: fmt::Write,
{
    write_table_internal(table, |line| writeln!(out, "{line}"))
}

fn write_table_internal<E, F>(table: &Table, mut write_line: F) -> Result<(), E>
where
    F: FnMut(&str) -> Result<(), E>,
{
    if table.columns.is_empty() {
        return Ok(());
    }

    let widths = compute_widths(table);
    let header = format_row(
        &table
            .columns
            .iter()
            .map(|column| column.header.as_str())
            .collect::<Vec<_>>(),
        &table.columns,
        &widths,
    );
    write_line(&header)?;

    for row in &table.rows {
        let cells = table
            .columns
            .iter()
            .enumerate()
            .map(|(index, _)| row.get(index).map_or("", String::as_str))
            .collect::<Vec<_>>();
        let line = format_row(&cells, &table.columns, &widths);
        write_line(&line)?;
    }

    Ok(())
}

fn compute_widths(table: &Table) -> Vec<usize> {
    table
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let mut width = column.header.len().max(column.min_width);
            for row in &table.rows {
                if let Some(cell) = row.get(index) {
                    width = width.max(cell.len());
                }
            }
            width
        })
        .collect()
}

fn format_row(cells: &[&str], columns: &[TableColumn], widths: &[usize]) -> String {
    let mut line = String::new();
    for (index, ((cell, column), width)) in cells
        .iter()
        .zip(columns.iter())
        .zip(widths.iter())
        .enumerate()
    {
        if index > 0 {
            line.push(' ');
        }
        if index + 1 == columns.len() {
            line.push_str(cell);
            continue;
        }
        line.push_str(&aligned_cell(cell, *width, column.align));
    }
    line
}

fn aligned_cell(cell: &str, width: usize, align: TableAlign) -> String {
    let len = cell.len();
    if len >= width {
        return cell.to_string();
    }

    let padding = width - len;
    match align {
        TableAlign::Left => format!("{cell}{}", " ".repeat(padding)),
        TableAlign::Right => format!("{}{cell}", " ".repeat(padding)),
        TableAlign::Center => {
            let left = padding / 2;
            let right = padding - left;
            format!("{}{}{}", " ".repeat(left), cell, " ".repeat(right))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Table, TableAlign, TableColumn, write_table, write_table_fmt};

    #[test]
    fn writes_left_aligned_table_to_io_writer() {
        let table = Table::new(vec![
            TableColumn::new("ID").min_width(4),
            TableColumn::new("ROLE"),
        ])
        .with_rows(vec![vec!["abc".to_string(), "owner".to_string()]]);
        let mut out = Vec::new();
        write_table(&mut out, &table).expect("table should write");
        let text = String::from_utf8(out).expect("output should be utf8");
        assert_eq!(text, "ID   ROLE\nabc  owner\n");
    }

    #[test]
    fn writes_right_and_center_aligned_cells() {
        let table = Table::new(vec![
            TableColumn::new("A").align(TableAlign::Right).min_width(3),
            TableColumn::new("B").align(TableAlign::Center).min_width(5),
        ])
        .with_rows(vec![vec!["7".to_string(), "x".to_string()]]);
        let mut out = String::new();
        write_table_fmt(&mut out, &table).expect("table should write");
        assert_eq!(out, "  A B\n  7 x\n");
    }

    #[test]
    fn writes_only_headers_when_no_rows() {
        let table = Table::new(vec![TableColumn::new("ONE"), TableColumn::new("TWO")]);
        let mut out = Vec::new();
        write_table(&mut out, &table).expect("table should write");
        let text = String::from_utf8(out).expect("output should be utf8");
        assert_eq!(text, "ONE TWO\n");
    }
}
