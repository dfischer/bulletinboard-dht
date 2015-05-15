use std::thread::{spawn,sleep_ms};
use std::sync::mpsc::{Sender,Receiver,channel};
use std::sync::{Arc,Mutex};
use std::str;
use std::io;
use std::net::{SocketAddr,UdpSocket};
use std::collections::HashMap;

use rustc_serialize::json::{encode,decode};

use utils::semaphore::Semaphore;
use message::{Message, Cookie};
use node::Node;

fn ignore<R,E>(res: Result<R,E>) {
	match res {
		_ => ()
	}
}

pub struct Server {
	sock: UdpSocket,
	pending_requests: Arc<Mutex<HashMap<(SocketAddr, Cookie), Sender<Message>>>>
}

impl Clone for Server {
	fn clone(&self) -> Server {
		Server {
			sock:             self.sock.try_clone().unwrap(),
			pending_requests: self.pending_requests.clone(),
		}
	}
}

impl Server {
	pub fn new(sock: UdpSocket) -> Server {
		Server {
			sock: sock.try_clone().unwrap(),
			pending_requests: Arc::new(Mutex::new(HashMap::new())),
		}
	}

	pub fn local_addr(&self) -> io::Result<SocketAddr> {
		self.sock.local_addr()
	}

	pub fn send_request(&self, addr: SocketAddr, req: &Message) -> Message
	{
		let (tx, rx) = channel();

		{
			let mut pending = self.pending_requests.lock().unwrap();
			let key = (addr, req.cookie().unwrap().clone());
			(*pending).insert(key, tx);
		}

		let buf = encode(&req).unwrap().into_bytes();
		self.sock.send_to(&buf[..], addr).unwrap();

		rx.recv().unwrap()
	}

	pub fn send_response(&self, addr: SocketAddr, resp: &Message)
	{
		let buf = encode(&resp).unwrap().into_bytes();
		self.sock.send_to(&buf[..], addr).unwrap();
	}

	pub fn send_request_ms(&self, addr: &SocketAddr, req: &Message, timeout: u32)
		-> Message
	{
		let (tx, rx) = channel();

		{
			let mut pending = self.pending_requests.lock().unwrap();
			let key = (addr.clone(), req.cookie().unwrap().clone());
			(*pending).insert(key, tx.clone());
		}

		let buf = encode(&req).unwrap().into_bytes();
		self.sock.send_to(&buf[..], addr).unwrap();

		spawn(move || {
			sleep_ms(timeout);
			match tx.send(Message::Timeout) {
				Ok(_) => (),
				Err(_) => (),
			}
		});

		rx.recv().unwrap()
	}

	/// returns an Channel you can use as an Iterator of type [(addr_index, Message), ...]
	///
	/// just consume it until you got a reponse that satisfies your requirements
	/// (You probably do not want to call iter.collect(): it will ask ALL nodes!)
	pub fn send_many_request<I>(&self, iter: I, req: Message,
	                    timeout: u32, concurrency: isize)
		-> Receiver<(Node, Message)>
			where I: 'static + Iterator<Item=Node> + Send
	{
		let is_rx_dead = Arc::new(Mutex::new(false));
		let (tx, rx) = channel();

		let this = self.clone();
		spawn(move || {
			let sem = Arc::new(Semaphore::new(concurrency));

			for node in iter.take_while(|_| *(is_rx_dead.lock().unwrap()) == false) {
				let is_rx_dead = is_rx_dead.clone();
				let node = node.clone();
				let this = this.clone();
				let req = req.clone();
				let sem = sem.clone();
				let tx = tx.clone();

				// acquire BEFORE we spawn!
				sem.acquire();

				spawn(move || {
					let resp = this.send_request_ms(&node.addr, &req, timeout);
					sem.release();
					
					if tx.send((node,resp)).is_err() {
						*(is_rx_dead.lock().unwrap()) = true;
					}						
				});
			}
		});

		rx
	}
}

impl Iterator for Server {
	type Item = (SocketAddr, Message);

	fn next(&mut self) -> Option<Self::Item> {
		let mut buf = [0; 64*1024];

		loop {
			let (len, src) = self.sock.recv_from(&mut buf).unwrap();
			let msg = str::from_utf8(&buf[..len]);

			if msg.is_err() {
				continue
			}

			let msg:Result<Message,_> = decode(msg.unwrap());

			// dispatch responses
			match msg {
				Ok(Message::Ping(_))
				| Ok(Message::FindNode(_))
				| Ok(Message::FindValue(_))
				| Ok(Message::Store(_))
				| Ok(Message::Timeout)
				| Err(_) => (),

				Ok(ref resp @ Message::Pong(_))
				| Ok(ref resp @ Message::FoundNode(_))
				| Ok(ref resp @ Message::FoundValue(_)) => {
					let key = (src, resp.cookie().unwrap().clone());
					let mut pending = self.pending_requests.lock().unwrap();
					
					match (*pending).remove(&key) {
						None => (),
						Some(tx) => ignore(tx.send(resp.clone())),
					}
				},
			}

			match msg {
				Err(_) | Ok(Message::Timeout) => (),
				Ok(r) => return Some((src, r)),
			}
		}
	}
}