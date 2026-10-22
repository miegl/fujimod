//! Synthetic seqlock publication through a different CPU view of canonical RAM.
use super::super::{bus, factory};
use fujimod_emulator_cpu::{Model, Register, arm, timeline::Ratio};
use fujimod_emulator_cpu_unicorn::rprofile;

#[test]
fn cached_read_watch_wakes_for_every_uncached_publication_store() {
    use super::super::memory::BusMapping;
    use factory::{Engine, Graph, MemoryMap, Options, Processor, Ram};
    let maps = vec![
        MemoryMap {
            core: 0,
            address: 0x10000,
            physical: 0x10000,
            size: 0x1000,
            permissions: 7,
        },
        MemoryMap {
            core: 0,
            address: 0x40010000,
            physical: 0x10000,
            size: 0x1000,
            permissions: 7,
        },
        MemoryMap {
            core: 0,
            address: 0x20000,
            physical: 0x20000,
            size: 0x1000,
            permissions: 7,
        },
    ];
    let graph = Graph {
        processors: vec![Processor {
            model: Model::CortexR5F,
            released: true,
            mprofile: None,
            rprofile: Some(rprofile::core::Options {
                cp14_identity: 0,
                tcm: rprofile::tcm::Options {
                    atcm: rprofile::tcm::BankOptions {
                        bytes: 0x1000,
                        reset: 0xffff0001,
                    },
                    btcm: Default::default(),
                },
            }),
            registers: vec![(Register::Pc, 0x20000), (Register::Pstate, 0xd3)],
        }],
        ram: vec![
            Ram {
                base: 0x10000,
                size: 0x1000,
            },
            Ram {
                base: 0x20000,
                size: 0x1000,
            },
        ],
        mappings: maps.clone(),
        dma_mappings: vec![
            BusMapping::ram(0x10000, 0x1000),
            BusMapping::ram(0x20000, 0x1000),
            BusMapping {
                address: 0x40010000,
                physical: 0x10000,
                size: 0x1000,
                permissions: 3,
            },
        ],
        decoders: vec![],
        peripherals: vec![],
        instructions_per_tick: Ratio::new(1, 1).unwrap(),
    };
    let mut board = graph
        .build(
            Options {
                engine: Engine::Unicorn,
                translation_cache_bytes: 1024 * 1024,
            },
            bus::Bus::new(bus::Mode::Strict),
            None,
        )
        .unwrap();
    let code: [u32; 8] = [
        0xe3a00001, // mov r0,#1: publication busy
        0xe5810000, // str r0,[r1]: begin
        0xe5810008, // str r0,[r1,#8]: end
        0xe5810004, // str r0,[r1,#4]: publication number precedes completion
        0xe3a00002, // mov r0,#2: publication complete
        0xe5810008, // str r0,[r1,#8]: end
        0xe5810000, // str r0,[r1]: begin
        0xeafffffe,
    ];
    board
        .write_physical(
            0x20000,
            &code
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
    board.write_register(0, arm::CORE[1], 0x40010000).unwrap();
    let mut observed = vec![];
    loop {
        board.read_set_begin().unwrap();
        let mut bytes = [0; 12];
        board.read_physical(0x10000, &mut bytes).unwrap();
        let reads = board.read_set_end().unwrap();
        assert_eq!(reads.stores, vec![(0x10000, 0x1000c)]);
        let words: Vec<_> = bytes
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        observed.push(words.clone());
        if words[0] != 0 && words[0] == words[2] && words[0] & 1 == 0 {
            break;
        }
        assert!(observed.len() <= 5, "completed snapshot was never observed");
        board
            .continuation_begin(reads.stores, reads.devices, maps.clone())
            .unwrap();
        board.run_for_instructions(100, 7).unwrap();
        assert!(
            board.continuation_end().unwrap(),
            "alias store did not wake the predicate"
        );
    }
    assert_eq!(
        observed,
        vec![
            vec![0, 0, 0],
            vec![1, 0, 0],
            vec![1, 0, 1],
            vec![1, 1, 1],
            vec![1, 1, 2],
            vec![2, 1, 2]
        ]
    );
    assert_eq!(board.instruction_counts(), vec![7]);
}
