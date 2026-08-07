//! Detached eager writer for searchable-PDF output.
//!
//! Immutable extraction never reaches into a `lopdf::Document`; this isolated writer is the
//! named exception. It reparses the original bytes exactly once into a private mutable document,
//! applies OCR content, saves owned bytes, and drops the document before returning.

use crate::access::DocumentAccess;
use crate::ocr;
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};
use std::collections::HashMap;

pub(crate) fn build(
    access: &dyn DocumentAccess,
    results: &HashMap<u32, String>,
    remove_raster: bool,
) -> Result<Vec<u8>, String> {
    let source_length = access.source_len().map_err(|error| error.to_string())?;
    let raw = access
        .materialize_source_bounded(source_length)
        .map_err(|error| error.to_string())?;
    let mut document = crate::doc::load_mem_deterministic(&raw).map_err(|error| error.to_string())?;
    let (helv, helv_b) = ocr::pdf::add_fonts(&mut document);
    let pages = document.get_pages();
    for (&page_number, &page_id) in &pages {
        let Some(doctags) = results.get(&page_number) else { continue };
        let (width, height) = ocr::page_size_pts(access, page_id);
        if remove_raster {
            replace_page(&mut document, access, page_id, doctags, width, height, helv, helv_b)?;
        } else {
            overlay_page(&mut document, page_id, doctags, width, height, helv, helv_b)?;
        }
    }
    if remove_raster {
        document.prune_objects();
    }
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes)
}

fn replace_page(
    document: &mut Document,
    access: &dyn DocumentAccess,
    page_id: ObjectId,
    doctags: &str,
    width: f32,
    height: f32,
    helv: ObjectId,
    helv_b: ObjectId,
) -> Result<(), String> {
    let image = ocr::page_main_image(access, page_id).map(|(_, image)| image);
    let input = ocr::pdf::PageInput {
        page: ocr::doctags::parse(doctags),
        width,
        height,
        image,
    };
    let (content, xobjects) = ocr::pdf::build_page_content(document, &input)?;
    let data = content.encode().map_err(|error| error.to_string())?;
    let stream_id = document.add_object(Stream::new(Dictionary::new(), data));
    let mut xobject_dict = Dictionary::new();
    for (name, id) in &xobjects {
        xobject_dict.set(name.as_bytes().to_vec(), Object::Reference(*id));
    }
    let resources = dictionary! {
        "Font" => dictionary! { "F1" => helv, "F2" => helv_b },
        "XObject" => xobject_dict,
    };
    let page = document
        .get_object_mut(page_id)
        .map_err(|error| error.to_string())?
        .as_dict_mut()
        .map_err(|error| error.to_string())?;
    page.set("Contents", Object::Reference(stream_id));
    page.set("Resources", Object::Dictionary(resources));
    Ok(())
}

fn overlay_page(
    document: &mut Document,
    page_id: ObjectId,
    doctags: &str,
    width: f32,
    height: f32,
    helv: ObjectId,
    helv_b: ObjectId,
) -> Result<(), String> {
    let input = ocr::pdf::PageInput {
        page: ocr::doctags::parse(doctags),
        width,
        height,
        image: None,
    };
    let data = ocr::pdf::build_text_overlay(&input)
        .encode()
        .map_err(|error| error.to_string())?;
    let stream_id = document.add_object(Stream::new(Dictionary::new(), data));
    append_page_content(document, page_id, stream_id);
    add_overlay_fonts(document, page_id, helv, helv_b);
    Ok(())
}

fn append_page_content(document: &mut Document, page_id: ObjectId, stream_id: ObjectId) {
    let Ok(page) = document.get_object_mut(page_id).and_then(|object| object.as_dict_mut()) else {
        return;
    };
    let contents = match page.get(b"Contents").ok().cloned() {
        Some(Object::Array(mut entries)) => {
            entries.push(Object::Reference(stream_id));
            Object::Array(entries)
        }
        Some(existing @ Object::Reference(_)) => {
            Object::Array(vec![existing, Object::Reference(stream_id)])
        }
        _ => Object::Reference(stream_id),
    };
    page.set("Contents", contents);
}

fn add_overlay_fonts(
    document: &mut Document,
    page_id: ObjectId,
    helv: ObjectId,
    helv_b: ObjectId,
) {
    let mut resources = match document.get_page_resources(page_id) {
        Ok((Some(dictionary), _)) => dictionary.clone(),
        Ok((None, ids)) => ids
            .first()
            .and_then(|id| document.get_dictionary(*id).ok())
            .cloned()
            .unwrap_or_default(),
        Err(_) => Dictionary::new(),
    };
    let mut fonts = match resources.get(b"Font").ok().cloned() {
        Some(Object::Dictionary(dictionary)) => dictionary,
        Some(Object::Reference(id)) => document.get_dictionary(id).cloned().unwrap_or_default(),
        _ => Dictionary::new(),
    };
    fonts.set(ocr::pdf::OVERLAY_FONT, Object::Reference(helv));
    fonts.set(ocr::pdf::OVERLAY_FONT_BOLD, Object::Reference(helv_b));
    resources.set("Font", fonts);
    if let Ok(page) = document.get_object_mut(page_id).and_then(|object| object.as_dict_mut()) {
        page.set("Resources", Object::Dictionary(resources));
    }
}
