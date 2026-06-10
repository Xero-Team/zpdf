use std::path::PathBuf;

fn main() {
    std::process::exit(match parse_args(std::env::args_os()) {
        Ok(path) => match zpdf_viewer_gpui::run(path) {
            Ok(()) => 0,
            Err(err) => {
                eprintln!("{err}");
                1
            }
        },
        Err(code) => code,
    });
}

fn parse_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Result<PathBuf, i32> {
    let mut args = args.into_iter();
    let _program = args.next();
    let Some(path) = args.next() else {
        eprintln!("usage: zpdf-viewer-gpui <file.pdf>");
        return Err(1);
    };
    if args.next().is_some() {
        eprintln!("usage: zpdf-viewer-gpui <file.pdf>");
        return Err(1);
    }
    Ok(PathBuf::from(path))
}
