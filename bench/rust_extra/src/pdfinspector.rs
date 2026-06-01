use std::time::Instant;
use std::path::Path;
fn median(mut v: Vec<f64>) -> f64 { v.sort_by(|a,b| a.partial_cmp(b).unwrap()); v[v.len()/2] }
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let out = &args[0];
    println!("{{");
    let pdfs = &args[1..];
    for (i, p) in pdfs.iter().enumerate() {
        let name = Path::new(p).file_stem().unwrap().to_string_lossy().to_string();
        let res = pdf_inspector::process_pdf(p);
        let c = if i+1 < pdfs.len() {","} else {""};
        match res {
            Ok(r) => {
                let md = r.markdown.clone().unwrap_or_default();
                let mut ts = vec![];
                for _ in 0..5 { let t = Instant::now(); let _ = pdf_inspector::process_pdf(p); ts.push(t.elapsed().as_secs_f64()); }
                let _ = std::fs::write(format!("{out}/{name}.pdfinspector.md"), &md);
                println!("  \"{name}\": {{\"ok\": true, \"t_s\": {:.4}, \"chars\": {}, \"type\": {:?}, \"enc_issues\": {}, \"ocr_pages\": {}, \"layout\": {:?}}}{c}",
                    median(ts), md.len(), format!("{:?}", r.pdf_type), r.has_encoding_issues, r.pages_needing_ocr.len(), format!("{:?}", r.layout));
            }
            Err(e) => println!("  \"{name}\": {{\"ok\": false, \"err\": {:?}}}{c}", format!("{e}")),
        }
    }
    println!("}}");
}
