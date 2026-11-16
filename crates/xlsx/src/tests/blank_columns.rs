//! A column entry that carries nothing (the default width, no style or the default style, shown,
//! no outline) is not written: the reader drops such an entry, so writing it would make a save of
//! the reopened workbook differ from the first save. A column's default style is the same as no
//! style, since columns are the last place a cell's style falls back to.

use std::sync::Arc;

use gridcraft_model::{LineInfo, Sheet, StyleId, Workbook};

use crate::{read_xlsx, write_xlsx};

fn sheet_xml(bytes: &[u8]) -> String {
    String::from_utf8(crate::package::Package::open(bytes).unwrap().read("xl/worksheets/sheet1.xml").unwrap().unwrap()).unwrap()
}

#[test]
fn blank_column_entries_are_not_written() {
    let mut wb = Workbook::new();
    wb.sheets = vec![Arc::new(Sheet::new("Sheet1"))];
    let sh = wb.sheet_mut(0).unwrap();
    sh.cols.insert(2, LineInfo { style: Some(StyleId::DEFAULT), ..LineInfo::default() });
    sh.cols.insert(3, LineInfo::default());
    let first = write_xlsx(&wb).unwrap();
    let xml = sheet_xml(&first);
    assert!(!xml.contains("<cols"), "{xml}");
    let (back, _) = read_xlsx(&first).unwrap();
    assert_eq!(write_xlsx(&back).unwrap(), first, "write, reopen, write gives the same bytes");
}

#[test]
fn a_column_that_carries_something_is_still_written() {
    let mut wb = Workbook::new();
    wb.sheets = vec![Arc::new(Sheet::new("Sheet1"))];
    let sh = wb.sheet_mut(0).unwrap();
    sh.cols.insert(1, LineInfo { style: Some(StyleId::DEFAULT), ..LineInfo::default() });
    sh.cols.insert(2, LineInfo { hidden: true, style: Some(StyleId::DEFAULT), ..LineInfo::default() });
    sh.cols.insert(4, LineInfo { size: Some(120.0), ..LineInfo::default() });
    let first = write_xlsx(&wb).unwrap();
    let xml = sheet_xml(&first);
    assert!(xml.contains(r#"<cols><col min="3" max="3" width="9.140625" hidden="1"/><col min="5" max="5""#), "{xml}");
    let (back, _) = read_xlsx(&first).unwrap();
    assert_eq!(write_xlsx(&back).unwrap(), first, "write, reopen, write gives the same bytes");
}
