//! Prove the native reading matches the real machine, not a container's view.
fn main() {
    kmplify_node::hostcpu::start();
    for i in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let c = kmplify_node::hostcpu::snapshot();
        println!(
            "sample {i}: {:.1}% | {} physical / {} logical | sampled={} | {}",
            c.percent, c.physical_cores, c.logical_cores, c.sampled, c.model
        );
    }
}
