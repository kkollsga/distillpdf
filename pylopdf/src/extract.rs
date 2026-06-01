//! Image and font extraction pillars, built on lopdf's object model.

use lopdf::{Dictionary, Document, Object};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

fn filter_to_format(filters: &Option<Vec<String>>) -> &'static str {
    match filters {
        Some(fs) => {
            if fs.iter().any(|f| f == "DCTDecode") {
                "jpeg"
            } else if fs.iter().any(|f| f == "JPXDecode") {
                "jpx"
            } else if fs.iter().any(|f| f == "CCITTFaxDecode") {
                "ccitt"
            } else if fs.iter().any(|f| f == "JBIG2Decode") {
                "jbig2"
            } else {
                "raw" // Flate/LZW/none -> needs PNG assembly from samples
            }
        }
        None => "raw",
    }
}

/// Extract images from all pages as a list of dicts:
/// {page, index, width, height, color_space, format, data(bytes)}.
pub fn extract_images<'py>(py: Python<'py>, doc: &Document) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for (&pno, &page_id) in &doc.get_pages() {
        let imgs = match doc.get_page_images(page_id) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (idx, im) in imgs.iter().enumerate() {
            let d = PyDict::new(py);
            d.set_item("page", pno)?;
            d.set_item("index", idx)?;
            d.set_item("width", im.width)?;
            d.set_item("height", im.height)?;
            d.set_item("color_space", im.color_space.clone())?;
            d.set_item("format", filter_to_format(&im.filters))?;
            d.set_item("data", PyBytes::new(py, im.content))?;
            list.append(d)?;
        }
    }
    Ok(list)
}

/// Resolve an object that may be a direct value or an indirect reference.
fn resolve<'a>(doc: &'a Document, obj: &'a Object) -> Option<&'a Object> {
    match obj {
        Object::Reference(r) => doc.get_object(*r).ok(),
        other => Some(other),
    }
}

/// Does this font dict (or its descendant) carry an embedded font program?
fn font_embedded(doc: &Document, dict: &Dictionary) -> bool {
    // Type0: descriptor lives on the descendant font.
    let descriptor = dict
        .get(b"FontDescriptor")
        .ok()
        .and_then(|o| resolve(doc, o))
        .or_else(|| {
            dict.get(b"DescendantFonts")
                .ok()
                .and_then(|o| resolve(doc, o))
                .and_then(|o| o.as_array().ok())
                .and_then(|a| a.first())
                .and_then(|o| resolve(doc, o))
                .and_then(|o| o.as_dict().ok())
                .and_then(|dd| dd.get(b"FontDescriptor").ok())
                .and_then(|o| resolve(doc, o))
        });
    match descriptor.and_then(|o| o.as_dict().ok()) {
        Some(d) => {
            d.has(b"FontFile") || d.has(b"FontFile2") || d.has(b"FontFile3")
        }
        None => false,
    }
}

/// Extract per-page font info: {page, name, subtype, base_font, encoding,
/// embedded(bool), has_tounicode(bool)}.
pub fn extract_fonts<'py>(py: Python<'py>, doc: &Document) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for (&pno, &page_id) in &doc.get_pages() {
        let fonts = match doc.get_page_fonts(page_id) {
            Ok(f) => f,
            Err(_) => continue,
        };
        for (name, dict) in fonts {
            let d = PyDict::new(py);
            d.set_item("page", pno)?;
            d.set_item("name", String::from_utf8_lossy(&name).into_owned())?;
            let subtype = dict
                .get(b"Subtype")
                .and_then(|o| o.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            d.set_item("subtype", subtype)?;
            let base_font = dict
                .get(b"BaseFont")
                .and_then(|o| o.as_name())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_default();
            d.set_item("base_font", base_font)?;
            let encoding = dict
                .get(b"Encoding")
                .ok()
                .and_then(|o| o.as_name().ok())
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .unwrap_or_else(|| "custom".to_string());
            d.set_item("encoding", encoding)?;
            d.set_item("embedded", font_embedded(doc, dict))?;
            d.set_item("has_tounicode", dict.has(b"ToUnicode"))?;
            list.append(d)?;
        }
    }
    Ok(list)
}
