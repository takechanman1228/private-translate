use crate::{Dictionary, Document, Error, Object, ObjectId, Result};
#[cfg(feature = "font_embedding")]
use crate::{FontData, Stream};

impl Document {
    /// Create new PDF document with version.
    pub fn with_version<S: Into<String>>(version: S) -> Document {
        let mut document = Self::new();
        document.version = version.into();
        document
    }

    /// Create an object ID.
    ///
    /// # Panics
    ///
    /// Panics if [`Document::max_id`] is already `u32::MAX`. Use
    /// [`Document::try_new_object_id`] to get an error instead.
    pub fn new_object_id(&mut self) -> ObjectId {
        self.try_new_object_id().unwrap_or_else(|err| panic!("{err}"))
    }

    /// Create an object ID, or return [`Error::ObjectIdOverflow`] if [`Document::max_id`] is
    /// already `u32::MAX`.
    pub fn try_new_object_id(&mut self) -> Result<ObjectId> {
        self.max_id = self.max_id.checked_add(1).ok_or(Error::ObjectIdOverflow)?;
        Ok((self.max_id, 0))
    }

    /// Add PDF object into document's object list.
    ///
    /// # Panics
    ///
    /// Panics if [`Document::max_id`] is already `u32::MAX`. Use
    /// [`Document::try_add_object`] to get an error instead.
    pub fn add_object<T: Into<Object>>(&mut self, object: T) -> ObjectId {
        self.try_add_object(object).unwrap_or_else(|err| panic!("{err}"))
    }

    /// Add PDF object into document's object list, or return [`Error::ObjectIdOverflow`] if
    /// [`Document::max_id`] is already `u32::MAX`.
    pub fn try_add_object<T: Into<Object>>(&mut self, object: T) -> Result<ObjectId> {
        let id = self.try_new_object_id()?;
        self.objects.insert(id, object.into());
        Ok(id)
    }

    pub fn set_object<T: Into<Object>>(&mut self, id: ObjectId, object: T) {
        self.objects.insert(id, object.into());
    }

    /// Remove PDF object from document's object list.
    ///
    /// Other objects may still hold references to this object! Therefore, removing the object might
    /// lead to dangling references.
    pub fn remove_object(&mut self, object_id: &ObjectId) -> Result<()> {
        self.objects.remove(object_id);
        Ok(())
    }

    /// Remove annotation from the document.
    ///
    /// References to this annotation are removed from the pages' lists of annotations. Finally, the
    /// annotation object itself is removed.
    pub fn remove_annot(&mut self, object_id: &ObjectId) -> Result<()> {
        for (_, page_id) in self.get_pages() {
            // `/Annots` may be missing, an array in the page, or a reference to an array.
            let annots_id = match self.get_dictionary(page_id).and_then(|page| page.get(b"Annots")) {
                Ok(Object::Reference(id)) => Some(*id),
                Ok(Object::Array(_)) => None,
                _ => continue,
            };
            let annots = match annots_id {
                Some(id) => self.get_object_mut(id).and_then(Object::as_array_mut),
                None => self
                    .get_dictionary_mut(page_id)
                    .and_then(|page| page.get_mut(b"Annots"))
                    .and_then(Object::as_array_mut),
            };
            let Ok(annots) = annots else {
                continue;
            };

            annots.retain(|object| {
                if let Ok(id) = object.as_reference() {
                    return id != *object_id;
                }

                true
            });
        }

        self.remove_object(object_id)?;

        Ok(())
    }

    /// Get the page's resource dictionary.
    ///
    /// Get Object that has the key "Resources".
    pub fn get_or_create_resources(&mut self, page_id: ObjectId) -> Result<&mut Object> {
        let resources_id = {
            let page = self.get_object(page_id).and_then(Object::as_dict)?;
            if page.has(b"Resources") {
                page.get(b"Resources").and_then(Object::as_reference).ok()
            } else {
                None
            }
        };
        if let Some(res_id) = resources_id {
            return self.get_object_mut(res_id);
        }
        let page = self.get_object_mut(page_id).and_then(Object::as_dict_mut)?;
        if !page.has(b"Resources") {
            page.set(b"Resources", Dictionary::new());
        }
        page.get_mut(b"Resources")
    }

    /// Add XObject to a page.
    ///
    /// Get Object that has the key `Resources -> XObject`.
    pub fn add_xobject<N: Into<Vec<u8>>>(
        &mut self, page_id: ObjectId, xobject_name: N, xobject_id: ObjectId,
    ) -> Result<()> {
        if let Ok(resources) = self.get_or_create_resources(page_id).and_then(Object::as_dict_mut) {
            if !resources.has(b"XObject") {
                resources.set("XObject", Dictionary::new());
            }
            let mut xobjects = resources.get_mut(b"XObject")?;
            if let Object::Reference(xobjects_ref_id) = xobjects {
                let mut xobjects_id = *xobjects_ref_id;
                while let Object::Reference(id) = self.get_object(xobjects_id)? {
                    xobjects_id = *id;
                }
                xobjects = self.get_object_mut(xobjects_id)?;
            }
            let xobjects = Object::as_dict_mut(xobjects)?;
            xobjects.set(xobject_name, Object::Reference(xobject_id));
        }
        Ok(())
    }

    /// Add Graphics State to a page.
    ///
    /// Get Object that has the key `Resources -> ExtGState`.
    pub fn add_graphics_state<N: Into<Vec<u8>>>(
        &mut self, page_id: ObjectId, gs_name: N, gs_id: ObjectId,
    ) -> Result<()> {
        if let Ok(resources) = self.get_or_create_resources(page_id).and_then(Object::as_dict_mut) {
            if !resources.has(b"ExtGState") {
                resources.set("ExtGState", Dictionary::new());
            }
            let states = resources.get_mut(b"ExtGState").and_then(Object::as_dict_mut)?;
            states.set(gs_name, Object::Reference(gs_id));
        }
        Ok(())
    }

    /// Add font to a page.
    /// # Examples
    ///
    /// ```no_run
    /// // Assuming you have a font file at "./SomeFont.ttf"
    /// use lopdf::dictionary;
    ///
    /// let font_file = std::fs::read("./SomeFont.ttf").unwrap();
    ///
    /// // Create a new FontData instance with the font file.
    /// let font_name = "SomeFont".to_string();
    /// let mut font_data = lopdf::FontData::new(&font_file, font_name.clone());
    ///
    /// // Customize the font data if needed.
    /// font_data
    ///     .set_italic_angle(10)
    ///     .set_encoding("WinAnsiEncoding".to_string());
    ///
    ///
    /// // Create a new PDF document.
    /// let mut doc = lopdf::Document::with_version("1.5");
    ///
    /// // Add the font to the document.
    /// let font_id = doc.add_font(font_data).unwrap();
    ///
    /// // Now you can use `font_id` to reference the font in your PDF content.
    /// // For example:
    /// let resources_id = doc.add_object(dictionary! {
    ///  "Font" => dictionary! {
    ///         font_name => font_id,
    ///     },
    /// });
    /// ```
    #[cfg(feature = "font_embedding")]
    pub fn add_font(&mut self, font_data: FontData) -> Result<ObjectId> {
        // Create embedded font stream
        let font_stream = Stream::new(
            dictionary! {
                "Length1" => Object::Integer(font_data.bytes().len() as i64),
            },
            font_data.bytes(),
        );
        let font_file_id = self.add_object(font_stream);
        let font_name = font_data.font_name.clone();

        // Create font descriptor dictionary
        let font_descriptor_id = self.add_object(dictionary! {
            "Type" => "FontDescriptor",
            "FontName" => Object::Name(font_name.clone().into_bytes()),
            "Flags" => Object::Integer(font_data.flags),
            "FontBBox" => Object::Array(vec![
                Object::Integer(font_data.font_bbox.0),
                Object::Integer(font_data.font_bbox.1),
                Object::Integer(font_data.font_bbox.2),
                Object::Integer(font_data.font_bbox.3),
            ]),
            "ItalicAngle" => Object::Integer(font_data.italic_angle),
            "Ascent" => Object::Integer(font_data.ascent),
            "Descent" => Object::Integer(font_data.descent),
            "CapHeight" => Object::Integer(font_data.cap_height),
            "StemV" => Object::Integer(font_data.stem_v),
            "FontFile2" => Object::Reference(font_file_id),
        });

        // Create font dictionary
        let font_id = self.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "TrueType",
            "BaseFont" => Object::Name(font_name.clone().into_bytes()),
            "FontDescriptor" => Object::Reference(font_descriptor_id),
            "Encoding" => Object::Name(font_data.encoding.into_bytes()),
        });

        if let Some(first_char) = font_data.first_char {
            self.get_dictionary_mut(font_id)
                .unwrap()
                .set("FirstChar", Object::Integer(first_char));
        }
        if let Some(last_char) = font_data.last_char {
            self.get_dictionary_mut(font_id)
                .unwrap()
                .set("LastChar", Object::Integer(last_char));
        }
        if let Some(widths_array) = font_data.widths {
            let widths = widths_array.into_iter().map(Object::Integer).collect();
            self.get_dictionary_mut(font_id)
                .unwrap()
                .set("Widths", Object::Array(widths));
        }

        Ok(font_id)
    }
}
