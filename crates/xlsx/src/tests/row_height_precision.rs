//! A row height is written precisely enough to read back as the same model height. Heights in
//! whole pixels keep their usual two-decimal point sizes (20 px is `15`), but an automatic height
//! the engine computed to a fraction of a pixel (32.727272 px for a 24 pt font) is written with
//! the digits it needs, or a save and reopen would change it (to 32.733334 px from `24.55`).

use std::sync::Arc;

use gridcraft_model::{LineInfo, Sheet, Workbook};

use crate::{read_xlsx, write_xlsx};

fn sheet_xml(bytes: &[u8]) -> String {
    String::from_utf8(crate::package::Package::open(bytes).unwrap().read("xl/worksheets/sheet1.xml").unwrap().unwrap()).unwrap()
}

#[test]
fn row_heights_read_back_exactly() {
    let heights: [f32; 7] = [21.0, 24.0, 32.727272, 25.454546, 43.636364, 15.333333, 100.12345];
    let mut wb = Workbook::new();
    wb.sheets = vec![Arc::new(Sheet::new("Sheet1"))];
    let sh = wb.sheet_mut(0).unwrap();
    for (i, h) in heights.iter().enumerate() {
        sh.rows.insert(i as u32 * 2, LineInfo { size: Some(*h), ..LineInfo::default() });
        sh.rows.insert(i as u32 * 2 + 1, LineInfo { size: Some(*h), custom: true, ..LineInfo::default() });
    }
    let first = write_xlsx(&wb).unwrap();
    let xml = sheet_xml(&first);
    assert!(xml.contains(r#"<row r="1" ht="15.75"/>"#), "whole pixels keep their usual form: {xml}");
    assert!(xml.contains(r#"<row r="4" ht="18" customHeight="1"/>"#), "{xml}");
    let (back, _) = read_xlsx(&first).unwrap();
    let got = back.sheet(0).unwrap();
    for (i, h) in heights.iter().enumerate() {
        for row in [i as u32 * 2, i as u32 * 2 + 1] {
            let size = got.rows.get(&row).and_then(|r| r.size);
            assert_eq!(size.map(f32::to_bits), Some(h.to_bits()), "row {} ({h} px) read back as {size:?}", row + 1);
        }
    }
    assert_eq!(write_xlsx(&back).unwrap(), first, "write, reopen, write gives the same bytes");
}
