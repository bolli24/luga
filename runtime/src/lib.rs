#![no_std]
#![no_main]

unsafe extern "C" {
    fn printf(fmt: *const i8, ...) -> i32;
}

#[unsafe(no_mangle)]
pub extern "C" fn hello_rust() {
    let msg = c"Hello from Rust!\n";
    unsafe {
        printf(msg.as_ptr());
    }
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
