use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let out_dir = env::var_os("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("categories_compiled.json");

    let macros_content = fs::read_to_string("src/classify/macros.json").expect("Failed to read macros.json");
    
    let mut categories = String::new();
    let cat_dir = fs::read_dir("src/classify/categories").expect("Failed to read categories dir");

    // Sort for deterministic order (matches Lua categoryDefinitions ordering by filename)
    let mut entries: Vec<_> = cat_dir.map(|e| e.unwrap()).collect();
    entries.sort_by_key(|e| e.file_name());

    let mut first = true;
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            let file_stem = path.file_stem().unwrap().to_str().unwrap();
            let content = fs::read_to_string(&path).unwrap();
            
            // We need to inject "id": "file_stem" into the JSON object
            let mut obj: serde_json::Value = serde_json::from_str(&content).unwrap();
            if let serde_json::Value::Object(ref mut map) = obj {
                map.insert("id".to_string(), serde_json::Value::String(file_stem.to_string()));
            }
            let modified = serde_json::to_string(&obj).unwrap();

            if !first {
                categories.push_str(",\n");
            }
            categories.push_str(&modified);
            first = false;
        }
    }

    let result = format!("{{ \"macros\": {}, \"categories\": [{}] }}", macros_content, categories);
    fs::write(&dest_path, result).unwrap();

    println!("cargo:rerun-if-changed=src/classify/macros.json");
    println!("cargo:rerun-if-changed=src/classify/categories");
}
