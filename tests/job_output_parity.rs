use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;

#[test]
fn polled_job_writing_past_the_pipe_buffer_exits_with_its_whole_output() {
    let mut child = Command::new("/bin/sh")
        .args([
            "-c",
            "head -c 1048576 /dev/zero; head -c 1048576 /dev/zero >&2",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = kilnr::runtime::drain(child.stdout.take().unwrap());
    let stderr = kilnr::runtime::drain(child.stderr.take().unwrap());
    let clock = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if clock.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            panic!("a job writing 1 MiB per stream never exited while polled");
        }
        thread::sleep(Duration::from_millis(10))
    };
    assert!(status.success());
    assert_eq!(stdout.join().unwrap().unwrap().len(), MIB);
    assert_eq!(stderr.join().unwrap().unwrap().len(), MIB);
}
