fn main() {
    tauri_build::build();

    guard_release_dev_mode();

    // Windows: Embed Common Controls v6 manifest for test binaries
    //
    // When running `cargo test`, the generated test executables don't include
    // the standard Tauri application manifest. Without Common Controls v6,
    // `tauri::test` calls fail with STATUS_ENTRYPOINT_NOT_FOUND.
    //
    // This workaround:
    // 1. Embeds the manifest into test binaries via /MANIFEST:EMBED
    // 2. Uses /MANIFEST:NO for the main binary to avoid duplicate resources
    //    (Tauri already handles manifest embedding for the app binary)
    #[cfg(target_os = "windows")]
    {
        let manifest_path = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"),
        )
        .join("common-controls.manifest");
        let manifest_arg = format!("/MANIFESTINPUT:{}", manifest_path.display());

        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg={}", manifest_arg);
        // Avoid duplicate manifest resources in binary builds.
        println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
        println!("cargo:rerun-if-changed={}", manifest_path.display());
    }
}

/// 防呆护栏：拦截「release 目录下的 dev 模式产物」。
///
/// tauri 判定 dev/prod 与 debug/release profile 无关，只看 tauri crate 的
/// `custom-protocol` feature（经 links="Tauri" 传播为 DEP_TAURI_DEV）。
/// 直接 `cargo build --release`（不经 `tauri build` CLI）时该 feature 未启用，
/// 产物运行时加载 devUrl（http://localhost:3000）而非内嵌前端——即 2026-10-01
/// 「localhost 拒绝连接」白屏事故的根因（trim.7 安装包即此产物）。
///
/// - `tauri build`（CI/发版路径）自动启用 custom-protocol，不受影响；
/// - debug 构建（cargo test / tauri dev）不受影响；
/// - 确需 dev 模式 release 产物时，设 CC_SWITCH_ALLOW_DEV_RELEASE=1 绕过。
fn guard_release_dev_mode() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    if std::env::var("CC_SWITCH_ALLOW_DEV_RELEASE").as_deref() == Ok("1") {
        println!(
            "cargo:warning=CC Switch: 允许 dev 模式 release 构建（CC_SWITCH_ALLOW_DEV_RELEASE=1），产物将加载 devUrl，禁止用于分发包"
        );
        return;
    }
    let dep_tauri_dev = std::env::var("DEP_TAURI_DEV").unwrap_or_default();
    if dep_tauri_dev == "true" {
        panic!(
            "\n\n[cc-switch] 检测到无效的 release 构建：未启用 custom-protocol，\
             产物将加载 devUrl (http://localhost:3000) 而非内嵌前端（桌面端表现为 \
             “localhost 拒绝连接”白屏）。\n\n\
             正确做法（任选其一）：\n  \
             1. pnpm tauri build            —— 推荐，CLI 自动启用 custom-protocol\n  \
             2. cargo build --release --features custom-protocol\n  \
             3. 确需 dev 模式 release 产物：设置环境变量 CC_SWITCH_ALLOW_DEV_RELEASE=1\n"
        );
    }
}
