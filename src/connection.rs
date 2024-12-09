// use std::sync::atomic::AtomicU32;

// struct Connection {
//     next_stream_id: AtomicU32,
//     streams: HashMap<u32, Stream>,
// }

// impl Session {
//     fn new() -> Self {
//         Session {
//             next_stream_id: AtomicU32::new(1),
//             streams: HashMap::new(),
//         }
//     }

//     fn next_stream_id(&self) -> u32 {
//         self.next_stream_id.fetch_add(2, Ordering::Relaxed)
//     }
// }