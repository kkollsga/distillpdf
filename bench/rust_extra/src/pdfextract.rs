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
        let (txt, ok) = match pdf_extract::extract_text(p) {
            Ok(t) => (t, true),
            Err(_) => (String::new(), false),
        };
        let mut ts = vec![];
        if ok {
            let _ = pdf_extract::extract_text(p);
            for _ in 0..5 { let t = Instant::now(); let _ = pdf_extract::extract_text(p); ts.push(t.elapsed().as_secs_f64()); }
            let _ = std::fs::write(format!("{out}/{name}.pdfextract.txt"), &txt);
        }
        let c = if i+1 < pdfs.len() {","} else {""};
        println!("  \"{name}\": {{\"ok\": {ok}, \"t_s\": {:.4}, \"chars\": {}}}{c}",
            if ts.is_empty(){0.0}else{median(ts)}, txt.len());
    }
    println!("}}");
}
