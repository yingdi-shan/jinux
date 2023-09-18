use log::info;

pub mod block;

pub fn init() {
    // print all the input device to make sure input crate will compile
    for (name, _) in jinux_input::all_devices() {
        info!("Found Input device, name:{}", name);
    }

    self::block::init();
}
