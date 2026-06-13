use js_sys::{Array, Object, Promise, RegExp, Uint8Array};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{Cursor, Read, Write};
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------
#[derive(Debug, thiserror::Error)]
pub enum ZiperError {
    #[error("Zip error: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),
}

impl From<ZiperError> for JsValue {
    fn from(err: ZiperError) -> Self {
        JsError::new(&err.to_string()).into()
    }
}

// ---------------------------------------------------------------------------
// Internal file entry
// ---------------------------------------------------------------------------
#[derive(Clone)]
struct ZipEntry {
    name: String,
    dir: bool,
    data: Option<Vec<u8>>,
    comment: String,
    date: Option<chrono::DateTime<chrono::Utc>>,
    compression: CompressionMethod,
    unsafe_original_name: Option<String>,
}

impl ZipEntry {
    fn new_file(name: String, data: Vec<u8>) -> Self {
        Self {
            name,
            dir: false,
            data: Some(data),
            comment: String::new(),
            date: Some(chrono::Utc::now()),
            compression: CompressionMethod::Store,
            unsafe_original_name: None,
        }
    }

    fn new_dir(name: String) -> Self {
        let display_name = if name.ends_with('/') {
            name
        } else {
            format!("{}/", name)
        };
        Self {
            name: display_name,
            dir: true,
            data: None,
            comment: String::new(),
            date: Some(chrono::Utc::now()),
            compression: CompressionMethod::Store,
            unsafe_original_name: None,
        }
    }

    fn raw_data(&self) -> Vec<u8> {
        self.data.clone().unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Compression methods (matches JSZip: STORE, DEFLATE)
// ---------------------------------------------------------------------------
#[derive(Clone, Copy, PartialEq)]
enum CompressionMethod {
    Store,
    Deflate,
}

impl CompressionMethod {
    fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "DEFLATE" => CompressionMethod::Deflate,
            _ => CompressionMethod::Store,
        }
    }
}

// ---------------------------------------------------------------------------
// Options structs
// ---------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Default)]
struct FileOptions {
    compression: Option<String>,
    #[serde(rename = "compressionOptions")]
    compression_options: Option<CompressionOptions>,
    comment: Option<String>,
    date: Option<f64>,
    binary: Option<bool>,
    #[serde(rename = "optimizedBinaryString")]
    optimized_binary_string: Option<bool>,
    #[serde(rename = "createFolders")]
    create_folders: Option<bool>,
    #[serde(rename = "unixPermissions")]
    unix_permissions: Option<u32>,
    #[serde(rename = "dosPermissions")]
    dos_permissions: Option<u16>,
    dir: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
struct CompressionOptions {
    level: Option<i32>,
}

#[derive(Serialize, Deserialize, Default)]
struct GenerateOptions {
    #[serde(rename = "type")]
    output_type: Option<String>,
    compression: Option<String>,
    #[serde(rename = "compressionOptions")]
    compression_options: Option<CompressionOptions>,
    comment: Option<String>,
    #[serde(rename = "mimeType")]
    mime_type: Option<String>,
    platform: Option<String>,
    #[serde(rename = "streamFiles")]
    stream_files: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
struct LoadOptions {
    base64: Option<bool>,
    #[serde(rename = "checkCRC32")]
    check_crc32: Option<bool>,
    #[serde(rename = "optimizedBinaryString")]
    optimized_binary_string: Option<bool>,
    #[serde(rename = "createFolders")]
    create_folders: Option<bool>,
}

// ---------------------------------------------------------------------------
// ZipObject – represents a file inside the archive (mirrors JSZip ZipObject)
// ---------------------------------------------------------------------------
#[wasm_bindgen]
pub struct ZipObject {
    name: String,
    dir: bool,
    date: String,
    comment: String,
    unsafe_original_name: Option<String>,
    data: Option<Vec<u8>>,
}

fn zip_object_to_js(obj: &ZipObject) -> Result<JsValue, JsValue> {
    let js_obj = Object::new();
    js_sys::Reflect::set(&js_obj, &JsValue::from_str("name"), &JsValue::from_str(&obj.name))?;
    js_sys::Reflect::set(&js_obj, &JsValue::from_str("dir"), &JsValue::from_bool(obj.dir))?;
    js_sys::Reflect::set(&js_obj, &JsValue::from_str("date"), &JsValue::from_str(&obj.date))?;
    js_sys::Reflect::set(&js_obj, &JsValue::from_str("comment"), &JsValue::from_str(&obj.comment))?;
    if let Some(ref unsafe_name) = obj.unsafe_original_name {
        js_sys::Reflect::set(&js_obj, &JsValue::from_str("unsafeOriginalName"), &JsValue::from_str(unsafe_name))?;
    } else {
        js_sys::Reflect::set(&js_obj, &JsValue::from_str("unsafeOriginalName"), &JsValue::NULL)?;
    }
    if let Some(ref data) = obj.data {
        let u8arr = Uint8Array::from(data.as_slice());
        js_sys::Reflect::set(&js_obj, &JsValue::from_str("data"), &u8arr)?;
    } else {
        js_sys::Reflect::set(&js_obj, &JsValue::from_str("data"), &JsValue::NULL)?;
    }
    Ok(js_obj.into())
}

#[wasm_bindgen]
impl ZipObject {
    #[wasm_bindgen(getter)]
    pub fn name(&self) -> String {
        self.name.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn dir(&self) -> bool {
        self.dir
    }

    #[wasm_bindgen(getter)]
    pub fn date(&self) -> String {
        self.date.clone()
    }

    #[wasm_bindgen(getter)]
    pub fn comment(&self) -> String {
        self.comment.clone()
    }

    #[wasm_bindgen(getter, js_name = unsafeOriginalName)]
    pub fn unsafe_original_name(&self) -> Option<String> {
        self.unsafe_original_name.clone()
    }

    /// async(type) -> Promise<Uint8Array | String>
    pub fn async_(&self, type_: &str) -> Promise {
        let data = self.data.clone().unwrap_or_default();
        let type_str = type_.to_string();
        future_to_promise(async move {
            match type_str.as_str() {
                "uint8array" => Ok(Uint8Array::from(data.as_slice()).into()),
                "arraybuffer" => {
                    let buf = Uint8Array::from(data.as_slice());
                    Ok(buf.buffer().into())
                }
                "string" | "binarystring" => {
                    let s = String::from_utf8_lossy(&data).to_string();
                    Ok(JsValue::from(s))
                }
                "base64" => {
                    use base64::Engine;
                    let engine = base64::engine::general_purpose::STANDARD;
                    Ok(JsValue::from(engine.encode(&data)))
                }
                "array" => {
                    let arr = js_sys::Array::new();
                    for b in &data {
                        arr.push(&JsValue::from(*b));
                    }
                    Ok(arr.into())
                }
                "blob" => {
                    let u8arr = Uint8Array::from(data.as_slice());
                    let array = js_sys::Array::new();
                    array.push(&u8arr);
                    let options = web_sys::BlobPropertyBag::new();
                    options.set_type("application/octet-stream");
                    let blob = web_sys::Blob::new_with_buffer_source_sequence_and_options(
                        array.as_ref(),
                        &options,
                    )
                        .map_err(|_| JsValue::from_str("Failed to create Blob"))?;
                    Ok(blob.into())
                }
                "nodebuffer" => {
                    Ok(Uint8Array::from(data.as_slice()).into())
                }
                _ => Err(JsValue::from_str(&format!("Unsupported type: {}", type_str))),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Main JSZip-compatible class
// ---------------------------------------------------------------------------
#[wasm_bindgen]
pub struct JSZip {
    files: Rc<RefCell<HashMap<String, ZipEntry>>>,
    comment: String,
    current_folder: String,
}

#[wasm_bindgen]
impl JSZip {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            files: Rc::new(RefCell::new(HashMap::new())),
            comment: String::new(),
            current_folder: String::new(),
        }
    }

    // ---- file(name) -> ZipObject | null ----
    // ---- file(name, data [, options]) -> JSZip ----
    #[wasm_bindgen]
    pub fn file(&mut self, name: &str, data: Option<JsValue>, options: Option<JsValue>) -> Result<JsValue, JsValue> {
        let file_opts = if let Some(opts) = options {
            serde_wasm_bindgen::from_value::<FileOptions>(opts).unwrap_or_default()
        } else {
            FileOptions::default()
        };

        // Getter mode: file(name) returns ZipObject (wasm instance with async_ method)
        if data.is_none() {
            let full_name = self.resolve_path(name);
            if let Some(entry) = self.files.borrow().get(&full_name) {
                let zip_obj = ZipObject {
                    name: entry.name.clone(),
                    dir: entry.dir,
                    date: entry
                        .date
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default(),
                    comment: entry.comment.clone(),
                    unsafe_original_name: entry.unsafe_original_name.clone(),
                    data: entry.data.clone(),
                };
                return Ok(JsValue::from(zip_obj));
            }
            return Ok(JsValue::NULL);
        }

        // Setter mode: file(name, data) returns self
        let js_data = data.unwrap();
        let bytes = js_value_to_vec_u8(&js_data)?;

        let full_name = self.resolve_path(name);

        let compression = file_opts
            .compression
            .as_deref()
            .map(CompressionMethod::from_str)
            .unwrap_or(CompressionMethod::Store);

        let entry = ZipEntry::new_file(full_name.clone(), bytes);
        let entry = ZipEntry {
            name: full_name.clone(),
            compression,
            comment: file_opts.comment.unwrap_or_default(),
            ..entry
        };

        self.files.borrow_mut().insert(full_name.clone(), entry);

        // If createFolders is true, create intermediate folders
        if file_opts.create_folders.unwrap_or(false) {
            self.create_parent_folders(&full_name);
        }

        let self_ref = JsValue::from(self.clone());
        Ok(self_ref)
    }

    // ---- folder(name) -> JSZip (sub-folder context) ----
    // ---- folder(regex) -> Array<ZipObject> ----
    #[wasm_bindgen]
    pub fn folder(&mut self, name_or_regex: &JsValue) -> Result<JsValue, JsValue> {
        // Check if it's a RegExp
        if let Some(regex) = name_or_regex.dyn_ref::<RegExp>() {
            return self.filter_internal(regex, true);
        }

        let name = name_or_regex.as_string().unwrap_or_default();

        // Ensure folder name ends with /
        let folder_name = if name.ends_with('/') {
            name
        } else {
            format!("{}/", name)
        };

        // Create folder entry
        let full_name = self.resolve_path(&folder_name);
        let entry = ZipEntry::new_dir(full_name.clone());
        self.files.borrow_mut().entry(full_name.clone()).or_insert(entry);

        // Return a new JSZip scoped to this folder
        let mut sub_zip = self.clone();
        sub_zip.current_folder = full_name;
        let self_ref = JsValue::from(sub_zip);
        Ok(self_ref)
    }

    // ---- forEach(callback) ----
    #[wasm_bindgen(js_name = forEach)]
    pub fn for_each(&self, callback: &js_sys::Function) -> Result<(), JsValue> {
        let this = JsValue::NULL;
        for (name, entry) in self.files.borrow().iter() {
            let zip_obj = ZipObject {
                name: name.clone(),
                dir: entry.dir,
                date: entry
                    .date
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default(),
                comment: entry.comment.clone(),
                unsafe_original_name: entry.unsafe_original_name.clone(),
                data: None,
            };
            let value = zip_object_to_js(&zip_obj)?;
            callback.call2(&this, &JsValue::from_str(name), &value)?;
        }
        Ok(())
    }

    // ---- filter(predicate) -> Array ----
    #[wasm_bindgen]
    pub fn filter(&self, predicate: &js_sys::Function) -> Result<Array, JsValue> {
        let this = JsValue::NULL;
        let result = Array::new();
        for (name, entry) in self.files.borrow().iter() {
            let zip_obj = ZipObject {
                name: name.clone(),
                dir: entry.dir,
                date: entry
                    .date
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default(),
                comment: entry.comment.clone(),
                unsafe_original_name: entry.unsafe_original_name.clone(),
                data: None,
            };
            let value = zip_object_to_js(&zip_obj)?;
            let should_include = predicate.call2(&this, &JsValue::from_str(name), &value)?;
            if should_include.is_truthy() {
                result.push(&value);
            }
        }
        Ok(result)
    }

    // ---- remove(name) -> JSZip ----
    #[wasm_bindgen]
    pub fn remove(&mut self, name: &str) -> JsValue {
        let full_name = self.resolve_path(name);
        let prefix = if full_name.ends_with('/') {
            full_name.clone()
        } else {
            format!("{}/", full_name)
        };

        // Remove exact match and all children
        self.files.borrow_mut().retain(|k, _| k != &full_name && !k.starts_with(&prefix));

        JsValue::from(self.clone())
    }

    // ---- generateAsync(options) -> Promise ----
    #[wasm_bindgen(js_name = generateAsync)]
    pub fn generate_async(&self, options: JsValue, on_update: Option<js_sys::Function>) -> Promise {
        let opts = serde_wasm_bindgen::from_value::<GenerateOptions>(options).unwrap_or_default();
        let zip_clone = self.clone();
        let on_update = on_update.clone();

        future_to_promise(async move {
            let mut buf = Cursor::new(Vec::new());

            let global_compression = opts
                .compression
                .as_deref()
                .map(CompressionMethod::from_str);

            let total_files = zip_clone.files.borrow().len();
            let mut processed = 0;

            {
                let files = zip_clone.files.borrow();
                let mut sorted_entries: Vec<(&String, &ZipEntry)> = files.iter().collect();
                sorted_entries.sort_by(|a, b| {
                    let a_depth = a.0.matches('/').count();
                    let b_depth = b.0.matches('/').count();
                    match (a.1.dir, b.1.dir) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => a_depth.cmp(&b_depth).then_with(|| a.0.cmp(b.0)),
                    }
                });

                let mut zip_writer = zip::ZipWriter::new(&mut buf);

                for (name, entry) in sorted_entries {
                    if on_update.is_some() {
                        let percent = if total_files > 0 {
                            (processed as f64 / total_files as f64) * 100.0
                        } else {
                            0.0
                        };
                        if let Some(ref cb) = on_update {
                            let meta = js_sys::Object::new();
                            js_sys::Reflect::set(&meta, &"percent".into(), &JsValue::from(percent)).ok();
                            js_sys::Reflect::set(&meta, &"currentFile".into(), &JsValue::from(name)).ok();
                            cb.call1(&JsValue::NULL, &meta).ok();
                        }
                    }

                    let effective_compression = global_compression.unwrap_or(entry.compression);

                    let compression_method = match effective_compression {
                        CompressionMethod::Store => zip::CompressionMethod::Stored,
                        CompressionMethod::Deflate => zip::CompressionMethod::Deflated,
                    };

                    let file_options = zip::write::SimpleFileOptions::default()
                        .compression_method(compression_method);

                    if entry.dir {
                        zip_writer
                            .add_directory(name.clone(), file_options)
                            .map_err(|e| JsValue::from_str(&format!("Zip error: {}", e)))?;
                    } else {
                        let data = entry.raw_data();
                        zip_writer
                            .start_file(name.clone(), file_options)
                            .map_err(|e| JsValue::from_str(&format!("Zip error: {}", e)))?;
                        zip_writer
                            .write_all(&data)
                            .map_err(|e| JsValue::from_str(&format!("IO error: {}", e)))?;
                    }

                    processed += 1;
                }

                zip_writer
                    .finish()
                    .map_err(|e| JsValue::from_str(&format!("Zip finalize error: {}", e)))?;
            }

            let bytes = buf.into_inner();
            let output_type = opts.output_type.as_deref().unwrap_or("uint8array");

            match output_type {
                "uint8array" => Ok(Uint8Array::from(bytes.as_slice()).into()),
                "arraybuffer" => {
                    let u = Uint8Array::from(bytes.as_slice());
                    Ok(u.buffer().into())
                }
                "base64" => {
                    use base64::Engine;
                    let engine = base64::engine::general_purpose::STANDARD;
                    Ok(JsValue::from(engine.encode(&bytes)))
                }
                "string" | "binarystring" => {
                    let s = bytes.iter().map(|&b| b as char).collect::<String>();
                    Ok(JsValue::from(s))
                }
                "array" => {
                    let arr = js_sys::Array::new();
                    for b in &bytes {
                        arr.push(&JsValue::from(*b));
                    }
                    Ok(arr.into())
                }
                "blob" => {
                    let u8arr = Uint8Array::from(bytes.as_slice());
                    let array = js_sys::Array::new();
                    array.push(&u8arr);
                    let mime_type = opts.mime_type.as_deref().unwrap_or("application/zip");
                    let options = web_sys::BlobPropertyBag::new();
                    options.set_type(mime_type);
                    let blob = web_sys::Blob::new_with_buffer_source_sequence_and_options(
                        array.as_ref(),
                        &options,
                    )
                        .map_err(|_| JsValue::from_str("Failed to create Blob"))?;
                    Ok(blob.into())
                }
                "nodebuffer" => {
                    Ok(Uint8Array::from(bytes.as_slice()).into())
                }
                _ => Err(JsValue::from_str(&format!(
                    "Unsupported output type: {}",
                    output_type
                ))),
            }
        })
    }

    // ---- loadAsync(data [, options]) -> Promise<JSZip> ----
    #[wasm_bindgen(js_name = loadAsync)]
    pub fn load_async(&self, data: JsValue, options: Option<JsValue>) -> Promise {
        let load_opts = if let Some(opts) = options {
            serde_wasm_bindgen::from_value::<LoadOptions>(opts).unwrap_or_default()
        } else {
            LoadOptions::default()
        };

        let mut zip_clone = self.clone();
        // Give the clone an independent files map so loadAsync doesn't modify the original
        let files_copy = zip_clone.files.borrow().clone();
        zip_clone.files = Rc::new(RefCell::new(files_copy));

        future_to_promise(async move {
            let bytes = js_value_to_vec_u8(&data)?;

            let reader = Cursor::new(bytes);
            let mut archive = zip::ZipArchive::new(reader)
                .map_err(|e| JsValue::from_str(&format!("Failed to open zip: {}", e)))?;

            let create_folders = load_opts.create_folders.unwrap_or(false);

            for i in 0..archive.len() {
                let mut file = archive.by_index(i)
                    .map_err(|e| JsValue::from_str(&format!("Failed to read file {}: {}", i, e)))?;
                let name = file.name().to_string();

                // Sanitize path (prevent zip slip)
                let sanitized = sanitize_path(&name);

                if file.is_dir() {
                    let entry = ZipEntry::new_dir(sanitized.clone());
                    zip_clone.files.borrow_mut().insert(sanitized.clone(), entry);
                } else {
                    let mut content = Vec::new();
                    file.read_to_end(&mut content)
                        .map_err(|e| JsValue::from_str(&format!("Failed to read content: {}", e)))?;

                    let unsafe_name = if name != sanitized {
                        Some(name)
                    } else {
                        None
                    };

                    let entry = ZipEntry::new_file(sanitized.clone(), content);
                    zip_clone.files.borrow_mut().insert(
                        sanitized.clone(),
                        ZipEntry {
                            unsafe_original_name: unsafe_name,
                            ..entry
                        },
                    );
                }
            }

            let comment_bytes = archive.comment();
            if !comment_bytes.is_empty() {
                zip_clone.comment = String::from_utf8_lossy(comment_bytes).to_string();
            }

            // Create parent folders if requested
            if create_folders {
                let names: Vec<String> = zip_clone.files.borrow().keys().cloned().collect();
                for name in names {
                    zip_clone.create_parent_folders(&name);
                }
            }

            let self_ref = JsValue::from(zip_clone);
            Ok(self_ref)
        })
    }

    // ---- files getter ----
    #[wasm_bindgen(getter)]
    pub fn files(&self) -> Result<JsValue, JsValue> {
        let obj = Object::new();
        for (name, entry) in self.files.borrow().iter() {
            let zip_obj = ZipObject {
                name: name.clone(),
                dir: entry.dir,
                date: entry
                    .date
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default(),
                comment: entry.comment.clone(),
                unsafe_original_name: entry.unsafe_original_name.clone(),
                data: None,
            };
            let value = zip_object_to_js(&zip_obj)?;
            js_sys::Reflect::set(&obj, &JsValue::from_str(name), &value)?;
        }
        Ok(obj.into())
    }

    // ---- comment getter / setter ----
    #[wasm_bindgen(getter)]
    pub fn comment(&self) -> String {
        self.comment.clone()
    }

    #[wasm_bindgen(setter)]
    pub fn set_comment(&mut self, c: String) {
        self.comment = c;
    }
}

impl Clone for JSZip {
    fn clone(&self) -> Self {
        Self {
            files: self.files.clone(),
            comment: self.comment.clone(),
            current_folder: self.current_folder.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn js_value_to_vec_u8(value: &JsValue) -> Result<Vec<u8>, JsValue> {
    // Uint8Array
    if let Some(u8arr) = value.dyn_ref::<Uint8Array>() {
        let len = u8arr.length() as usize;
        let mut buf = vec![0u8; len];
        u8arr.copy_to(&mut buf);
        return Ok(buf);
    }

    // ArrayBuffer
    if let Some(buf) = value.dyn_ref::<js_sys::ArrayBuffer>() {
        let u8 = Uint8Array::new(buf);
        let len = u8.length() as usize;
        let mut data = vec![0u8; len];
        u8.copy_to(&mut data);
        return Ok(data);
    }

    // String (binary string or base64)
    if let Some(s) = value.as_string() {
        // Try base64 first
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD;
        if let Ok(decoded) = engine.decode(&s) {
            return Ok(decoded);
        }
        // Fall back to binary string (1 byte per char)
        return Ok(s.bytes().collect());
    }

    // Array of bytes
    if let Some(arr) = value.dyn_ref::<js_sys::Array>() {
        let len = arr.length() as usize;
        let mut buf = Vec::with_capacity(len);
        for i in 0..arr.length() {
            let val = arr.get(i);
            if let Some(n) = val.as_f64() {
                buf.push(n as u8);
            }
        }
        return Ok(buf);
    }

    Err(JsValue::from_str("Unsupported data type. Expected Uint8Array, ArrayBuffer, string, or Array."))
}

fn sanitize_path(path: &str) -> String {
    // Remove leading slashes
    let parts: Vec<&str> = path
        .trim_start_matches(|c| c == '/' || c == '\\')
        .split(&['/', '\\'])
        .collect();

    let mut result = Vec::new();
    for part in &parts {
        match *part {
            "" | "." => continue,
            ".." => {
                result.pop();
            }
            _ => result.push(*part),
        }
    }

    let is_dir = path.ends_with('/') || path.ends_with('\\');
    let sanitized = result.join("/");
    if is_dir && !sanitized.is_empty() {
        format!("{}/", sanitized)
    } else {
        sanitized
    }
}

impl JSZip {
    fn resolve_path(&self, name: &str) -> String {
        let raw = if name.starts_with('/') {
            name.trim_start_matches('/').to_string()
        } else if self.current_folder.is_empty() {
            name.to_string()
        } else {
            format!("{}{}", self.current_folder, name)
        };
        let parts: Vec<&str> = raw.split('/').collect();
        let cleaned: Vec<String> = parts
            .iter()
            .map(|p| {
                p.chars()
                    .filter(|c| !c.is_control())
                    .collect::<String>()
            })
            .collect();
        cleaned.join("/")
    }

    fn create_parent_folders(&mut self, path: &str) {
        let parts: Vec<&str> = path.split('/').collect();
        let mut current = String::new();
        for (i, part) in parts.iter().enumerate() {
            if part.is_empty() {
                continue;
            }
            if i < parts.len() - 1 {
                if current.is_empty() {
                    current = part.to_string();
                } else {
                    current = format!("{}/{}", current, part);
                }
                let folder_name = format!("{}/", current);
                self.files
                    .borrow_mut()
                    .entry(folder_name.clone())
                    .or_insert_with(|| ZipEntry::new_dir(folder_name));
            }
        }
    }

    fn filter_internal(&self, regex: &RegExp, only_folders: bool) -> Result<JsValue, JsValue> {
        let result = Array::new();
        for (name, entry) in self.files.borrow().iter() {
            if only_folders && !entry.dir {
                continue;
            }
            if regex.test(name) {
                let zip_obj = ZipObject {
                    name: name.clone(),
                    dir: entry.dir,
                    date: entry
                        .date
                        .map(|d| d.to_rfc3339())
                        .unwrap_or_default(),
                    comment: entry.comment.clone(),
                    unsafe_original_name: entry.unsafe_original_name.clone(),
                    data: None,
                };
                let value = zip_object_to_js(&zip_obj)?;
                result.push(&value);
            }
        }
        Ok(result.into())
    }
}

// ---------------------------------------------------------------------------
// Static version property
// ---------------------------------------------------------------------------
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
