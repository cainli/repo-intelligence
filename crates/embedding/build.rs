//! build.rs:首次 clone/CI 时自动下载打包模型(绕开 fastembed 5.x 经 hf-hub 下载的
//! Content-Range bug,改用 scripts/fetch-model.sh 直连 hf-mirror)。模型已存在则跳过;
//! 下载失败只发 cargo:warning(不中断 build),运行时 Embedder::new 会给出清晰错误。
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let model = manifest.join("models/all-MiniLM-L6-v2/model.onnx");
    if !model.exists() {
        let script = manifest.join("scripts/fetch-model.sh");
        println!("cargo:warning=[embedding] 模型缺失,自动下载(首次 ~23MB from hf-mirror)...");
        let ok = Command::new("bash")
            .arg(&script)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("cargo:warning=[embedding] 模型就绪。");
        } else {
            println!(
                "cargo:warning=[embedding] 模型下载失败。手动跑: bash crates/embedding/scripts/fetch-model.sh"
            );
        }
    }
    println!("cargo:rerun-if-changed=scripts/fetch-model.sh");
}
