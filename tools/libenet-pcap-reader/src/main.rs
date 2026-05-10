use std::{fs::File, io::Cursor, net::Ipv4Addr, path::PathBuf};

use clap::Parser;
use etherparse::{SlicedPacket, TransportSlice};
use libenet::decode_packet;
use pcap_file::pcap::PcapReader;

#[derive(Parser)]
struct Args {
    input: PathBuf,
    address: Ipv4Addr,
}

fn main() {
    let args = Args::parse();
    let file = File::open(args.input).unwrap();

    let mut pcap_reader = PcapReader::new(file).unwrap();

    'packet_loop: while let Some(pkt) = pcap_reader.next_packet() {
        let pkt = pkt.unwrap();
        let parsed = SlicedPacket::from_ip(&pkt.data).unwrap();

        let is_source = if let Some(net) = parsed.net {
            if let Some(ipv4) = net.ipv4_ref() {
                if ipv4.header().destination_addr() == args.address {
                    false
                } else if ipv4.header().source_addr() == args.address {
                    true
                } else {
                    continue 'packet_loop;
                }
            } else {
                continue 'packet_loop;
            }
        } else {
            continue 'packet_loop;
        };
        match parsed.transport {
            Some(TransportSlice::Udp(slice)) => {
                println!("{}", is_source);
                println!("{:?}", slice.payload());
                println!("{:?}", slice.payload().len());
                for msg in decode_packet(&mut Cursor::new(slice.payload())).unwrap() {
                    println!("{:?}", msg);
                }
            }
            _ => continue 'packet_loop,
        }
    }
}
