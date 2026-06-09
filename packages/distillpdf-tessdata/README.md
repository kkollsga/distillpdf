# distillpdf-tessdata

Optional Tesseract language data (`eng`, `por`, `nor`) for [distillPDF](https://github.com/kkollsga/distillpdf)'s
built-in fast OCR engine. The base `distillpdf` wheel ships **English** only; install this to
add more languages, fully offline:

```bash
pip install 'distillpdf[languages]'
```

distillPDF discovers this package automatically and points the Tesseract engine at its
`tessdata/` directory. The `.traineddata` files are the `tessdata_fast` LSTM models from the
[Tesseract project](https://github.com/tesseract-ocr/tessdata_fast) (Apache-2.0).
