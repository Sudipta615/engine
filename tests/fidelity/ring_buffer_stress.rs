//! SPSC Ring Buffer Concurrency & Stress Tests
//!
//! Verifies thread-safety, memory-order correctness (Acquire/Release),
//! wrap-around integrity, full/empty boundary transitions, and concurrent
//! producer/consumer ordering under heavy thread contention.

use std::sync::Arc;
use std::thread;

use engine::buffer::{AudioFrame, FixedFrameBuffer, PcmRingBuffer};

#[test]
fn test_fixed_frame_buffer_wraparound() {
    let buf = FixedFrameBuffer::new(128).unwrap();

    // Push and pop 50,000 frames to exercise ring pointer wraparound thoroughly
    for i in 0..50000 {
        let frame = AudioFrame::stereo(i as f32, -(i as f32));
        assert!(buf.push(frame), "Push must succeed when empty");
        let popped = buf.pop().expect("Pop must succeed when pushed");
        assert_eq!(popped.get(0), i as f32);
        assert_eq!(popped.get(1), -(i as f32));
    }
}

#[test]
fn test_pcm_ring_buffer_interleaved_slices() {
    let ring = PcmRingBuffer::new(512);

    // 128 frames x 2 channels = 256 interleaved samples
    let input_stereo: Vec<f32> = (0..128).flat_map(|i| [i as f32, (i * 2) as f32]).collect();
    let written = ring.write_interleaved(&input_stereo, 2);
    assert_eq!(written, 128); // 256 samples / 2 channels = 128 frames

    let mut output_stereo = vec![0.0f32; 256];
    let read = ring.read_interleaved(&mut output_stereo, 2);
    assert_eq!(read, 128);
    assert_eq!(output_stereo, input_stereo);
}

#[test]
fn test_pcm_ring_buffer_concurrent_spsc_stress() {
    const TOTAL_SAMPLES: usize = 1_000_000;
    const CHUNK_SIZE: usize = 256;
    let ring = Arc::new(PcmRingBuffer::<f32>::new(4096));

    let ring_producer = Arc::clone(&ring);
    let prod_handle = thread::spawn(move || {
        let mut sent = 0;
        let mut chunk = vec![0.0f32; CHUNK_SIZE];
        while sent < TOTAL_SAMPLES {
            let to_send = (TOTAL_SAMPLES - sent).min(CHUNK_SIZE);
            for (i, slot) in chunk.iter_mut().take(to_send).enumerate() {
                *slot = (sent + i) as f32;
            }
            let written = ring_producer.push_block(&chunk[..to_send]);
            sent += written;
            if written == 0 {
                thread::yield_now();
            }
        }
    });

    let ring_consumer = Arc::clone(&ring);
    let cons_handle = thread::spawn(move || {
        let mut received = 0;
        let mut read_buf = vec![0.0f32; CHUNK_SIZE];
        while received < TOTAL_SAMPLES {
            let got = ring_consumer.pop_block(&mut read_buf);
            for (i, &got_v) in read_buf.iter().take(got).enumerate() {
                let expected = (received + i) as f32;
                assert_eq!(
                    got_v,
                    expected,
                    "Sample mismatch at index {}: got {}, expected {}",
                    received + i,
                    got_v,
                    expected
                );
            }
            received += got;
            if got == 0 {
                thread::yield_now();
            }
        }
        received
    });

    prod_handle.join().unwrap();
    let total_received = cons_handle.join().unwrap();
    assert_eq!(total_received, TOTAL_SAMPLES);
}

#[test]
fn test_pcm_ring_buffer_consumer_lag_bursts() {
    // Producer generates data continuously, consumer sleeps/lags and reads in bursts
    const TOTAL_SAMPLES: usize = 200_000;
    let ring = Arc::new(PcmRingBuffer::<f32>::new(1024));

    let ring_p = Arc::clone(&ring);
    let prod = thread::spawn(move || {
        let mut sent = 0;
        let mut buf = vec![0.0f32; 128];
        while sent < TOTAL_SAMPLES {
            let n = (TOTAL_SAMPLES - sent).min(128);
            for (i, slot) in buf.iter_mut().take(n).enumerate() {
                *slot = (sent + i) as f32;
            }
            let written = ring_p.push_block(&buf[..n]);
            sent += written;
            if written == 0 {
                thread::yield_now();
            }
        }
    });

    let ring_c = Arc::clone(&ring);
    let cons = thread::spawn(move || {
        let mut received = 0;
        let mut buf = vec![0.0f32; 512];
        let mut iter = 0;
        while received < TOTAL_SAMPLES {
            iter += 1;
            if iter % 50 == 0 {
                // Simulate periodic consumer lag / scheduling pause
                thread::sleep(std::time::Duration::from_micros(100));
            }
            let got = ring_c.pop_block(&mut buf);
            for (i, &got_v) in buf.iter().take(got).enumerate() {
                let expected = (received + i) as f32;
                assert_eq!(got_v, expected);
            }
            received += got;
            if got == 0 {
                thread::yield_now();
            }
        }
        received
    });

    prod.join().unwrap();
    let total_received = cons.join().unwrap();
    assert_eq!(total_received, TOTAL_SAMPLES);
}

#[test]
fn test_pcm_ring_buffer_producer_lag_starvation() {
    // Consumer polls rapidly, producer starves consumer with intermittent bursts
    const TOTAL_SAMPLES: usize = 100_000;
    let ring = Arc::new(PcmRingBuffer::<f32>::new(1024));

    let ring_p = Arc::clone(&ring);
    let prod = thread::spawn(move || {
        let mut sent = 0;
        let mut buf = vec![0.0f32; 64];
        let mut iter = 0;
        while sent < TOTAL_SAMPLES {
            iter += 1;
            if iter % 20 == 0 {
                // Simulate producer lag / decoding delay
                thread::sleep(std::time::Duration::from_micros(100));
            }
            let n = (TOTAL_SAMPLES - sent).min(64);
            for (i, slot) in buf.iter_mut().take(n).enumerate() {
                *slot = (sent + i) as f32;
            }
            let written = ring_p.push_block(&buf[..n]);
            sent += written;
            if written == 0 {
                thread::yield_now();
            }
        }
    });

    let ring_c = Arc::clone(&ring);
    let cons = thread::spawn(move || {
        let mut received = 0;
        let mut buf = vec![0.0f32; 64];
        while received < TOTAL_SAMPLES {
            let got = ring_c.pop_block(&mut buf);
            for (i, &got_v) in buf.iter().take(got).enumerate() {
                let expected = (received + i) as f32;
                assert_eq!(got_v, expected);
            }
            received += got;
            if got == 0 {
                thread::yield_now();
            }
        }
        received
    });

    prod.join().unwrap();
    let total_received = cons.join().unwrap();
    assert_eq!(total_received, TOTAL_SAMPLES);
}

#[test]
fn test_fixed_frame_buffer_threaded_spsc() {
    const FRAMES: usize = 200_000;
    let buffer = Arc::new(FixedFrameBuffer::new(512).unwrap());

    let buf_p = Arc::clone(&buffer);
    let prod = thread::spawn(move || {
        for i in 0..FRAMES {
            let frame = AudioFrame::stereo(i as f32, (i * 2) as f32);
            while !buf_p.push(frame) {
                thread::yield_now();
            }
        }
    });

    let buf_c = Arc::clone(&buffer);
    let cons = thread::spawn(move || {
        let mut read_frames = 0;
        while read_frames < FRAMES {
            if let Some(frame) = buf_c.pop() {
                assert_eq!(frame.get(0), read_frames as f32);
                assert_eq!(frame.get(1), (read_frames * 2) as f32);
                read_frames += 1;
            } else {
                thread::yield_now();
            }
        }
        read_frames
    });

    prod.join().unwrap();
    let read = cons.join().unwrap();
    assert_eq!(read, FRAMES);
}

#[test]
fn test_pcm_ring_buffer_reset_and_bounds() {
    let ring = PcmRingBuffer::<f32>::new(64);
    assert_eq!(ring.available(), 0);
    assert_eq!(ring.free_slots(), 64);

    let data = vec![1.0f32; 64];
    let written = ring.push_block(&data);
    assert_eq!(written, 64);
    assert_eq!(ring.available(), 64);
    assert_eq!(ring.free_slots(), 0);

    // Further push should write 0 (full)
    assert_eq!(ring.push_block(&data), 0);

    // Reset clears everything atomically
    ring.reset();
    assert_eq!(ring.available(), 0);
    assert_eq!(ring.free_slots(), 64);

    // Pop on empty should return 0
    let mut out = vec![0.0f32; 16];
    assert_eq!(ring.pop_block(&mut out), 0);
}
