fn main() {
    // The icon lives in the resource table of the .exe: that is what Explorer, the
    // shortcuts, 发送到 and the "属性" dialog show **without** MxRun running. The
    // window / taskbar / tray icons are set at runtime from the same design (see
    // `window_icon` / `build_tray_icon` in src/main.rs).
    println!("cargo:rerun-if-changed=resources/mxrun.rc");
    println!("cargo:rerun-if-changed=icon/mxrun.ico");

    #[cfg(windows)]
    {
        // `-I icon` because the .rc refers to `mxrun.ico` by bare name.
        // A missing resource compiler (windres / rc.exe / llvm-rc) must not break
        // the build: the app still runs, it just loses the Explorer icon.
        if let Err(e) = embed_resource::compile("resources/mxrun.rc", embed_resource::ParamsIncludeDirs(["icon"]))
            .manifest_optional()
        {
            println!("cargo:warning=MxRun 图标资源未嵌入：{e:?}");
        }
    }
}
