//! Exercise the Block backend's guards against whatever this machine has.
fn main() {
    let devices = pyrographer_core::block::list().expect("list");
    for d in &devices {
        let refusal = pyrographer_core::block::write_refusal(d);
        println!(
            "{:<10} {:>16}B {:<8} refuse={:<5} mounts={:?}",
            d.name,
            d.bytes,
            d.bus.name(),
            d.carries_running_system,
            d.mounts
        );
        if let Some(why) = refusal {
            println!("           REFUSED: {why}");
        }
    }
    if let Some(target) = std::env::args().nth(1) {
        let Some(d) = devices.iter().find(|d| d.name == target) else {
            eprintln!("no such device {target}");
            return;
        };
        // `open` takes the widest access the device allows, decided at the
        // moment of the open: a write-protected card still opens, for reading.
        println!("\n--- opening {target} ---");
        match pyrographer_core::block::open(d) {
            Ok(mut agent) => {
                println!("opened for {:?}. info = {:?}", agent.access(), agent.info());
                let mut buf = vec![0u8; d.logical_block as usize];
                match agent.read_at(0, &mut buf) {
                    Ok(()) => println!("read sector 0: {:02x?}", &buf[..16.min(buf.len())]),
                    Err(e) => println!("read failed: {e}"),
                }
            }
            Err(e) => println!("open refused: {e}"),
        }
    }
}
