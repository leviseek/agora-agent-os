//! A spreadsheet, turned into rows a model can read.
//!
//! An .xlsx is a zip of XML parts: a shared string table, one XML file per sheet, and a workbook
//! that names them. That is the whole format as far as reading values is concerned, so this module
//! reads those parts directly rather than pulling in a spreadsheet engine - and it is deliberately
//! small: values and text, no formulas, no styling, no number formatting.
//!
//! What it produces is CSV text. Text is the point: the runtime already knows how to hand a model a
//! text table (see `documents`), and a model reads CSV far better than it reads a zip.

use agentos_core::error::{Result, RuntimeError};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::io::Read;

/// The zip magic. An .xlsx always starts with it, which is how a spreadsheet is told from text.
pub fn looks_like_zip(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06")
}

/// One sheet: the name the workbook gave it, and its rows.
#[derive(Debug, Clone)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<String>>,
}

/// A workbook that was read: its sheets, and the CSV text they turn into.
#[derive(Debug, Clone)]
pub struct Workbook {
    pub sheets: Vec<Sheet>,
    pub csv: String,
}

/// Read an .xlsx into rows.
///
/// Returns Ok(None) when the bytes are not a spreadsheet at all, so a caller falls through to the
/// text path instead of reporting a parse error for a file that is simply something else.
pub fn workbook_from_bytes(bytes: &[u8]) -> Result<Option<Workbook>> {
    if !looks_like_zip(bytes) {
        return Ok(None);
    }
    let mut archive = match zip::ZipArchive::new(std::io::Cursor::new(bytes)) {
        Ok(archive) => archive,
        // A zip that cannot be opened is not a spreadsheet; the caller decides what to say.
        Err(_) => return Ok(None),
    };
    let Some(workbook_xml) = read_entry(&mut archive, "xl/workbook.xml") else {
        // A plain zip, or a docx: not a spreadsheet.
        return Ok(None);
    };

    let names = parse_sheet_names(&workbook_xml);
    // The string table is optional: a workbook written without one stores its text inline.
    let shared: Vec<String> = match read_entry(&mut archive, "xl/sharedStrings.xml") {
        Some(xml) => parse_shared_strings(&xml)?,
        None => Vec::new(),
    };

    let mut sheets = Vec::new();
    // Sheet files are xl/worksheets/sheetN.xml. Their relationship to the workbook is declared in a
    // rels part, and in practice the order matches; reading them by index keeps this small, and a
    // workbook that breaks the convention still yields its data.
    for index in 1..=names.len().max(1) {
        let Some(xml) = read_entry(&mut archive, &format!("xl/worksheets/sheet{index}.xml")) else {
            continue;
        };
        let rows = parse_sheet(&xml, &shared)?;
        let name = names
            .get(index - 1)
            .cloned()
            .unwrap_or_else(|| format!("Sheet{index}"));
        sheets.push(Sheet { name, rows });
    }
    if sheets.is_empty() {
        return Err(RuntimeError::invalid_input(
            "the spreadsheet has no readable sheets: this reader takes cell values and text"
        ));
    }
    let csv = sheets_to_csv(&sheets);
    Ok(Some(Workbook { sheets, csv }))
}

/// Read one zip entry as text. A missing entry is None: every part except the workbook and the
/// sheets is optional.
fn read_entry<R: Read + std::io::Seek>(archive: &mut zip::ZipArchive<R>, name: &str) -> Option<String> {
    let mut entry = archive.by_name(name).ok()?;
    let mut text = String::new();
    entry.read_to_string(&mut text).ok()?;
    Some(text)
}

/// The shared string table: every string literal lives here, and a cell points at one by index.
/// Parsing it first is what makes a text cell readable.
fn parse_shared_strings(xml: &str) -> Result<Vec<String>> {
    let mut reader = Reader::from_str(xml);
    let mut strings = Vec::new();
    let mut current: Option<String> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => {
                if tag.name().as_ref() == b"si" {
                    current = Some(String::new());
                }
            }
            Ok(Event::Text(text)) => {
                if let Some(buffer) = current.as_mut() {
                    buffer.push_str(&text.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(tag)) => {
                if tag.name().as_ref() == b"si" {
                    strings.push(current.take().unwrap_or_default());
                }
            }
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(RuntimeError::invalid_input(format!(
                    "the spreadsheet's string table is not valid XML: {error}"
                )));
            }
            _ => {}
        }
    }
    Ok(strings)
}

/// The sheet names the workbook declares, in order.
fn parse_sheet_names(xml: &str) -> Vec<String> {
    let mut reader = Reader::from_str(xml);
    let mut names = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Empty(tag)) | Ok(Event::Start(tag)) => {
                if tag.name().as_ref() == b"sheet" {
                    for attribute in tag.attributes().flatten() {
                        if attribute.key.as_ref() == b"name" {
                            if let Ok(value) = attribute.unescape_value() {
                                names.push(value.to_string());
                            }
                        }
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    names
}

/// One worksheet, as rows of strings.
///
/// A cell knows its own address (r="C7") and empty cells are simply absent, so a row is rebuilt by
/// address rather than by position - otherwise a table with a blank column would silently shift.
fn parse_sheet(xml: &str, shared: &[String]) -> Result<Vec<Vec<String>>> {
    let mut reader = Reader::from_str(xml);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell: Option<(usize, String)> = None;
    let mut cell_type = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => match tag.name().as_ref() {
                b"row" => {
                    row = Vec::new();
                }
                b"c" => {
                    let mut reference = String::new();
                    cell_type.clear();
                    for attribute in tag.attributes().flatten() {
                        match attribute.key.as_ref() {
                            b"r" => {
                                reference = attribute
                                    .unescape_value()
                                    .unwrap_or_default()
                                    .to_string()
                            }
                            b"t" => {
                                cell_type = attribute
                                    .unescape_value()
                                    .unwrap_or_default()
                                    .to_string()
                            }
                            _ => {}
                        }
                    }
                    cell = Some((column_index(&reference), String::new()));
                }
                _ => {}
            },
            Ok(Event::Text(text)) => {
                if let Some((_, buffer)) = cell.as_mut() {
                    buffer.push_str(&text.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(tag)) => match tag.name().as_ref() {
                b"c" => {
                    if let Some((column, raw)) = cell.take() {
                        let value = match cell_type.as_str() {
                            // t="s": the value is an index into the shared string table.
                            "s" => raw
                                .trim()
                                .parse::<usize>()
                                .ok()
                                .and_then(|index| shared.get(index).cloned())
                                .unwrap_or_default(),
                            // Anything else is already the value: a number, a boolean, or an inline
                            // string.
                            _ => raw,
                        };
                        if row.len() <= column {
                            row.resize(column + 1, String::new());
                        }
                        row[column] = value;
                    }
                }
                b"row" => {
                    // A completely empty row is kept: it is a blank line in the table, and dropping
                    // it would shift every row after it.
                    rows.push(std::mem::take(&mut row));
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(error) => {
                return Err(RuntimeError::invalid_input(format!(
                    "a worksheet is not valid XML: {error}"
                )));
            }
            _ => {}
        }
    }
    Ok(rows)
}

/// "C7" -> 2. Only the letters matter; the row number is not needed once rows are separate.
fn column_index(reference: &str) -> usize {
    let mut index = 0usize;
    for character in reference.chars() {
        if !character.is_ascii_alphabetic() {
            break;
        }
        index = index * 26 + (character.to_ascii_uppercase() as usize - 'A' as usize + 1);
    }
    index.saturating_sub(1)
}

/// The sheets as CSV, one section each.
///
/// A field is quoted the RFC 4180 way, because a spreadsheet cell can contain a comma, a quote or a
/// newline - and the model has to still see the table it came from.
pub fn sheets_to_csv(sheets: &[Sheet]) -> String {
    let mut out = String::new();
    for (index, sheet) in sheets.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if sheets.len() > 1 {
            out.push_str(&format!("## sheet: {}\n", sheet.name));
        }
        for row in &sheet.rows {
            let line: Vec<String> = row.iter().map(|cell| quote(cell)).collect();
            out.push_str(&line.join(","));
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

fn quote(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A minimal .xlsx, written here rather than committed as a fixture: the reader and the file are
    /// checked against each other with nothing binary in the repository.
    fn workbook_bytes(sheet_xml: &str, shared_xml: &str) -> Vec<u8> {
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            let options: zip::write::FileOptions<'_, ()> =
                zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            let mut add = |name: &str, body: &str| {
                writer.start_file(name, options).unwrap();
                writer.write_all(body.as_bytes()).unwrap();
            };
            add(
                "xl/workbook.xml",
                r#"<?xml version="1.0"?><workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="销售" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
            );
            add("xl/sharedStrings.xml", shared_xml);
            add("xl/worksheets/sheet1.xml", sheet_xml);
            writer.finish().unwrap();
        }
        buffer
    }

    const SHARED: &str = r#"<?xml version="1.0"?><sst count="2" uniqueCount="2"><si><t>region</t></si><si><t>华东</t></si></sst>"#;

    #[test]
    fn a_spreadsheet_becomes_rows_and_a_named_sheet() {
        // A1 and B1 are text (shared strings), A2 is a string, B2 is a number, C2 is an inline
        // string, and row 1 has no third cell - the sparse case that breaks a reader which counts
        // cells instead of reading their addresses.
        let sheet = r#"<?xml version="1.0"?><worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="s"><v>1</v></c></row><row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2"><v>128000</v></c><c r="C2" t="inlineStr"><is><t>Q1</t></is></c></row></sheetData></worksheet>"#;
        let bytes = workbook_bytes(sheet, SHARED);
        let workbook = workbook_from_bytes(&bytes).unwrap().expect("a spreadsheet");
        assert_eq!(workbook.sheets.len(), 1);
        assert_eq!(workbook.sheets[0].name, "销售");
        let rows = &workbook.sheets[0].rows;
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0], vec!["region".to_string(), "华东".to_string()]);
        assert_eq!(rows[1][0], "华东");
        assert_eq!(rows[1][1], "128000");
        // Row 1 had no third cell: the row is what it is, and row 2 is padded to its own width.
        assert_eq!(rows[1].len(), 3, "{rows:?}");
        assert_eq!(rows[1][2], "Q1");
        assert!(workbook.csv.starts_with("region,华东"), "{}", workbook.csv);
    }

    #[test]
    fn a_cell_with_a_comma_is_quoted_so_the_table_still_reads() {
        let sheets = vec![Sheet {
            name: "s".into(),
            rows: vec![vec!["a,b".into(), "plain".into()]],
        }];
        assert_eq!(sheets_to_csv(&sheets), "\"a,b\",plain");
        let quoted = vec![Sheet { name: "s".into(), rows: vec![vec!["say \"hi\"".into()]] }];
        assert_eq!(sheets_to_csv(&quoted), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn a_zip_that_is_not_a_spreadsheet_is_not_claimed() {
        // Not a zip at all.
        assert!(workbook_from_bytes(b"region,revenue").unwrap().is_none());
        // A zip without a workbook part: a docx, or somebody's archive.
        let mut buffer = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buffer));
            writer
                .start_file("notes.txt", zip::write::FileOptions::<'_, ()>::default())
                .unwrap();
            writer.write_all(b"hello").unwrap();
            writer.finish().unwrap();
        }
        assert!(workbook_from_bytes(&buffer).unwrap().is_none());
    }

    #[test]
    fn the_column_letters_become_an_index() {
        assert_eq!(column_index("A1"), 0);
        assert_eq!(column_index("B2"), 1);
        assert_eq!(column_index("Z10"), 25);
        assert_eq!(column_index("AA1"), 26);
        assert_eq!(column_index(""), 0);
    }
}
