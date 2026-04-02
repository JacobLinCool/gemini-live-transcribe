use std::process::Command;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    for path in swift_runtime_paths() {
        emit_rpath(&path);
    }
}

fn swift_runtime_paths() -> Vec<String> {
    let mut paths = vec!["/usr/lib/swift".to_string()];

    let Ok(output) = Command::new("xcode-select").arg("-p").output() else {
        return paths;
    };

    if !output.status.success() {
        return paths;
    }

    let developer_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if developer_dir.is_empty() {
        return paths;
    }

    paths.push(format!(
        "{developer_dir}/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift-5.5/macosx"
    ));
    paths.push(format!(
        "{developer_dir}/Toolchains/XcodeDefault.xctoolchain/usr/lib/swift/macosx"
    ));

    paths
}

fn emit_rpath(path: &str) {
    println!("cargo:rustc-link-arg=-Wl,-rpath,{path}");
}
