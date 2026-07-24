extern crate winres;

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap() == "windows" {
        let major: u64 = std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap().parse().unwrap();
        let minor: u64 = std::env::var("CARGO_PKG_VERSION_MINOR").unwrap().parse().unwrap();
        let patch: u64 = std::env::var("CARGO_PKG_VERSION_PATCH").unwrap().parse().unwrap();
        let packed = (major << 48) | (minor << 32) | (patch << 16);

        let mut res = winres::WindowsResource::new();
        res.set_icon("rvci.ico");
        res.set("ProductName", "RVCI");
        res.set("FileDescription", "RVCI");
        res.set("CompanyName", "TZey");
        res.set("FileVersion", &format!("{}.{}.{}.0", major, minor, patch));
        res.set("ProductVersion", &format!("{}.{}.{}.0", major, minor, patch));
        res.set_version_info(winres::VersionInfo::FILEVERSION, packed);
        res.set_version_info(winres::VersionInfo::PRODUCTVERSION, packed);
        res.compile().unwrap();
    }
}
