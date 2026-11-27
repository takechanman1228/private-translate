//! Keep lopdf parsing, traversal, cloning and destruction off small UI worker stacks.
use lopdf::{Document, LoadOptions, StreamBudget, xref::XrefEntry};
use std::panic::{AssertUnwindSafe, catch_unwind};

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("protected PDF")]
    Protected,
    #[error(transparent)]
    Pdf(#[from] lopdf::Error),
    #[error("xref/object mismatch: {0}")]
    Incomplete(&'static str),
}

// Set from measurements of real object, xref, content and font streams.
pub(super) const STREAM_LIMIT: usize = 8 * 1024 * 1024;
const DOCUMENT_LIMIT: usize = 64 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static TEST_LIMITS: std::cell::Cell<(usize, usize)> =
        const { std::cell::Cell::new((STREAM_LIMIT, DOCUMENT_LIMIT)) };
}

pub(super) fn budget() -> StreamBudget {
    StreamBudget::current().unwrap_or_else(|| {
        #[cfg(test)]
        let (single, total) = TEST_LIMITS.get();
        #[cfg(not(test))]
        let (single, total) = (STREAM_LIMIT, DOCUMENT_LIMIT);
        StreamBudget::new(single, total)
    })
}

// lopdf 0.46 limits array/dictionary descent inside its grammar (including decoded
// object streams and operation operands) to 100. The stack is headroom for those
// bounded frames, not the bound itself. Rayon is disabled.
pub(super) fn with_document<T: Send, E: From<Error> + Send>(
    bytes: &[u8],
    use_document: impl FnOnce(Document) -> Result<T, E> + Send,
) -> Result<T, E> {
    let budget = budget();
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("pdf-parser".into())
            .stack_size(16 * 1024 * 1024)
            .spawn_scoped(scope, || {
                catch_unwind(AssertUnwindSafe(|| {
                    budget
                        .run(|| {
                            let doc = load(bytes)?;
                            budget.check().map_err(Error::Pdf)?;
                            use_document(doc)
                        })
                        .map_err(Error::Pdf)?
                }))
            })
            .map_err(|e| Error::Pdf(e.into()))?;
        worker
            .join()
            .and_then(|result| result)
            .map_err(|_| Error::Pdf(lopdf::Error::Syntax("PDF parser worker panicked".into())))?
    })
}

fn load(bytes: &[u8]) -> Result<Document, Error> {
    let doc = Document::load_mem_with_options(
        bytes,
        LoadOptions::with_max_decompressed_size(STREAM_LIMIT),
    )?;
    if doc.is_encrypted() || doc.encryption_state.is_some() {
        return Err(Error::Protected);
    }
    // Lenient loading can skip an unreadable object, including one
    // that exceeded the depth limit. Never work from a partial tree.
    if doc.xref_recovery.is_some() {
        return Err(Error::Incomplete("recovered cross-reference table"));
    }
    let mut expected = 0;
    for (&number, entry) in &doc.reference_table.entries {
        let generation = match entry {
            XrefEntry::Normal { generation, .. } => *generation,
            XrefEntry::Compressed { .. } => 0,
            _ => continue,
        };
        if !doc.objects.contains_key(&(number, generation)) {
            return Err(Error::Incomplete("missing object"));
        }
        expected += 1;
    }
    if expected != doc.objects.len() {
        return Err(Error::Incomplete("unexpected object"));
    }
    Ok(doc)
}

#[cfg(feature = "pdf-erase")]
pub(super) fn operation(bytes: &[u8]) -> lopdf::Result<lopdf::content::Content> {
    // Called by the eraser inside with_document. Consume the entire delimited span.
    lopdf::content::Content::decode_strict(bytes)
}

#[cfg(all(test, feature = "pdf-erase"))]
mod tests {
    use super::*;
    use crate::pdf::{Pdf, export};
    use lopdf::dictionary;
    use std::io::Write;

    fn sample_document() -> Document {
        let mut doc = Document::with_version("1.7");
        let pages = doc.new_object_id();
        let content = doc.add_object(lopdf::Stream::new(
            lopdf::Dictionary::new(),
            b"BT /F1 12 Tf 36 72 Td (Synthetic PDF) Tj ET".to_vec(),
        ));
        let page = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages,
            "MediaBox" => vec![0.into(), 0.into(), 300.into(), 120.into()],
            "Resources" => dictionary! {
                "Font" => dictionary! {
                    "F1" => dictionary! {
                        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
                    },
                },
            },
            "Contents" => content,
        });
        doc.set_object(
            pages,
            dictionary! {
                "Type" => "Pages", "Kids" => vec![page.into()], "Count" => 1,
            },
        );
        let root = doc.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages});
        doc.trailer.set("Root", root);
        doc
    }

    // Keep the page tree shallow and put the value in a separate ASCIIHex-encoded
    // object stream. A scan of the raw file cannot see the nesting. No fixture is
    // written outside the test's temporary directory.
    fn object_stream_sample(form: bool, dict: bool, depth: usize) -> Vec<u8> {
        let mut doc = sample_document();
        if form {
            doc.catalog_mut().unwrap().set(
                "AcroForm",
                dictionary! {"Fields" => Vec::<lopdf::Object>::new()},
            );
        }
        let mut pdf = Vec::new();
        doc.save_to(&mut pdf).unwrap();
        let base = load(&pdf).unwrap();
        let member = base.max_id + 1;
        let container = member + 1;
        let xref = member + 2;
        let header = format!("{member} 0 ");
        let (open, close) = if dict {
            ("3c3c2f5620", "3e3e")
        } else {
            ("5b", "5d")
        };
        let mut encoded = Vec::new();
        for byte in header.bytes() {
            write!(encoded, "{byte:02x}").unwrap();
        }
        encoded.extend(open.repeat(depth).bytes());
        encoded.extend(b"30");
        encoded.extend(close.repeat(depth).bytes());
        encoded.push(b'>');
        pdf.push(b'\n');
        let stream_offset = pdf.len() as u32;
        writeln!(pdf, "{container} 0 obj\n<< /Type /ObjStm /N 1 /First {} /Length {} /Filter /ASCIIHexDecode >>\nstream", header.len(), encoded.len()).unwrap();
        pdf.extend(encoded);
        pdf.extend(b"\nendstream\nendobj\n");
        let xref_offset = pdf.len() as u32;
        let (root, generation) = base.trailer.get(b"Root").unwrap().as_reference().unwrap();
        writeln!(pdf, "{xref} 0 obj\n<< /Type /XRef /Index [{member} 3] /W [1 4 1] /Size {} /Root {root} {generation} R /Prev {} /Length 18 >>\nstream", xref + 1, base.xref_start).unwrap();
        for (kind, offset) in [(2, container), (1, stream_offset), (1, xref_offset)] {
            pdf.push(kind);
            pdf.extend(offset.to_be_bytes());
            pdf.push(0);
        }
        writeln!(pdf, "\nendstream\nendobj\nstartxref\n{xref_offset}\n%%EOF").unwrap();
        pdf
    }

    #[test]
    #[ignore = "requires local PDFium and IPA font"]
    fn bounded_nesting_on_a_small_caller_stack() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let dir = tempfile::tempdir().unwrap();
                let input = dir.path().join("sample.pdf");
                for dict in [false, true] {
                    for form in [false, true] {
                        for depth in [1, 200, 500, 5_000, 100_000] {
                            let bytes = object_stream_sample(form, dict, depth);
                            std::fs::write(&input, &bytes).unwrap();
                            let rejected = depth > 1;
                            let parsed = with_document(&bytes, |_| Ok::<_, Error>(()));
                            if rejected {
                                assert!(matches!(parsed, Err(Error::Incomplete("missing object"))));
                            } else {
                                parsed.unwrap();
                            }
                            let admission = Pdf::read(&input);
                            let layout = export::Document::read(&input);
                            if rejected && form {
                                for e in [admission.err(), layout.err()] {
                                    assert!(matches!(
                                        e,
                                        Some(crate::FileError::Pdf(crate::pdf::Error::Interactive))
                                    ));
                                }
                            } else {
                                admission.unwrap();
                                let layout = layout.unwrap();
                                let targets =
                                    vec![Some("Translation".into()); layout.units().len()];
                                let saved = layout.write(&targets, "en").unwrap();
                                assert!(!saved.bytes.is_empty());
                                if rejected {
                                    assert!(saved.fallback_error.is_some());
                                }
                            }
                        }
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn raw_and_filtered_xref_streams_charge_the_balance() {
        for filtered in [false, true] {
            // One free entry, with three one-byte fields.
            let mut stream = lopdf::Stream::new(
                dictionary! {
                    "Type" => "XRef", "Size" => 1, "W" => vec![1.into(), 1.into(), 1.into()]
                },
                vec![0, 0, 0],
            );
            if filtered {
                stream.set_content(b"000000>".to_vec());
                stream.dict.set("Filter", "ASCIIHexDecode");
            }
            let decode = || lopdf::xref::decode_xref_stream(stream.clone());
            StreamBudget::new(3, 3).run(decode).unwrap().unwrap();
            assert!(matches!(
                StreamBudget::new(3, 2).run(decode),
                Err(lopdf::Error::Decompress(
                    lopdf::DecompressError::MemoryLimitExceeded { .. }
                ))
            ));
        }
    }

    #[test]
    fn filter_stages_share_the_document_balance() {
        // Two ASCIIHex layers produce three bytes ("41>") and then one ("A").
        let stream = lopdf::Stream::new(
            dictionary! {"Filter" => vec![lopdf::Object::Name(b"ASCIIHexDecode".to_vec()); 2]},
            b"34313E>".to_vec(),
        );
        assert_eq!(
            StreamBudget::new(3, 4)
                .run(|| stream.get_plain_content())
                .unwrap()
                .unwrap(),
            b"A"
        );
        for (single, total) in [(2, 4), (3, 3)] {
            let result = StreamBudget::new(single, total).run(|| stream.get_plain_content());
            assert!(matches!(
                result,
                Err(lopdf::Error::Decompress(
                    lopdf::DecompressError::MemoryLimitExceeded { .. }
                ))
            ));
        }
    }

    #[test]
    #[ignore = "requires local PDFium and IPA font"]
    fn stream_limits_reject_unverified_forms_and_preserve_bilingual_fallback() {
        // Use ordinary small object streams and lower only the test budget.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("budget.pdf");
        for form in [false, true] {
            let mut doc = sample_document();
            if form {
                doc.catalog_mut().unwrap().set(
                    "AcroForm",
                    dictionary! {"Fields" => Vec::<lopdf::Object>::new()},
                );
            }
            for _ in 0..8 {
                doc.add_object(dictionary! { "Description" => lopdf::Object::string_literal("A sample metadata entry. ".repeat(128)) });
            }
            let options = lopdf::SaveOptions::builder()
                .use_xref_streams(true)
                .use_object_streams(true)
                .max_objects_per_stream(1)
                .compression_level(6)
                .build();
            let mut bytes = Vec::new();
            doc.save_with_options(&mut bytes, options).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            for (single, total, exceeded) in [
                (2048, 65536, true),
                (8192, 16384, true),
                (8192, 4 * 1024 * 1024, false),
            ] {
                // This test thread owns the override. Production workers inherit the
                // same budget as their caller. Final output assembly gets a new one.
                TEST_LIMITS.set((single, total));
                let parsed = with_document(&bytes, |_| Ok::<_, Error>(()));
                if exceeded {
                    assert!(matches!(
                        parsed,
                        Err(Error::Pdf(lopdf::Error::Decompress(
                            lopdf::DecompressError::MemoryLimitExceeded { .. }
                        )))
                    ));
                } else {
                    parsed.unwrap();
                }
                let open = Pdf::read(&path);
                let layout = export::Document::read(&path);
                if form && exceeded {
                    for result in [open.err(), layout.err()] {
                        assert!(matches!(
                            result,
                            Some(crate::FileError::Pdf(crate::pdf::Error::Interactive))
                        ));
                    }
                } else {
                    open.unwrap();
                    let layout = layout.unwrap();
                    let targets = vec![Some("Translation".into()); layout.units().len()];
                    let saved = layout.write(&targets, "en").unwrap();
                    assert!(!saved.bytes.is_empty());
                    assert_eq!(
                        saved.fallback_error.is_some(),
                        exceeded,
                        "{:?}",
                        saved.fallback_error
                    );
                    if exceeded {
                        assert!(
                            saved
                                .pages
                                .iter()
                                .all(|p| p.kind == export::PageKind::Bilingual)
                        );
                        assert_eq!(
                            saved.fallback_error.as_deref(),
                            Some("cannot edit PDF: couldn't decompress stream")
                        );
                    }
                }
            }
            TEST_LIMITS.set((STREAM_LIMIT, DOCUMENT_LIMIT));
        }
    }

    #[test]
    fn parser_worker_panic_returns_an_error() {
        let mut bytes = Vec::new();
        sample_document().save_to(&mut bytes).unwrap();
        let result: Result<(), Error> = with_document(&bytes, |_| panic!("test worker"));
        assert!(matches!(result, Err(Error::Pdf(_))));
    }
}
