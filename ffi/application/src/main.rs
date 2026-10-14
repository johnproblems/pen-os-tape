mod concurrency;
mod debug;
mod heap;
mod runtime;
mod unreachable;
mod utilities;

use runtime::runtime;
use std::time::Duration;
use tokio::time::sleep;

ffi::import!(_pen_main, async fn() -> ffi::None);

// Provided by the os-tape static library linked into every application:
// `_pen_os_tape_init` parses `PEN_TAPE` before user code runs so startup
// failures abort before the program starts; `_pen_os_tape_finalize` flushes
// recordings and prints the final replay report. It returns the process exit
// code (non-zero when diff mode found differences).
unsafe extern "C" {
    fn _pen_os_tape_init();
    fn _pen_os_tape_finalize() -> i32;
}

fn main() {
    unsafe { _pen_os_tape_init() };
    runtime().block_on(async {
        _pen_main().await;

        // HACK Wait for all I/O buffers to be flushed (hopefully.)
        sleep(Duration::from_millis(50)).await;
    });

    let code = unsafe { _pen_os_tape_finalize() };
    if code != 0 {
        std::process::exit(code);
    }
}
