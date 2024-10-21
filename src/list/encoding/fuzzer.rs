use rand::prelude::*;
use crate::list::{ListCRDT, ListOpLog};
use crate::list::encoding::EncodeOptions;
use crate::list::old_fuzzer_tools::old_make_random_change;
use crate::list_fuzzer_tools::{choose_2, make_random_change};
use crate::listmerge::simple_oplog::{SimpleBranch, SimpleOpLog};

// This fuzzer will make an oplog, spam it with random changes from a single peer. Then save & load
// it back to make sure the result doesn't change.
fn fuzz_encode_decode_once(seed: u64) {
    let mut doc = ListCRDT::new();
    doc.get_or_create_agent_id_from_str("a"); // 0
    doc.get_or_create_agent_id_from_str("b"); // 1
    doc.get_or_create_agent_id_from_str("c"); // 2

    let mut rng = SmallRng::seed_from_u64(seed);

    for _i in 0..300 {
        // println!("\n\nIteration {i}");
        let agent = rng.gen_range(0..3);
        for _k in 0..rng.gen_range(1..=3) {
            old_make_random_change(&mut doc, None, agent, &mut rng, true);
        }

        let bytes = doc.oplog.encode(&EncodeOptions::full().store_deleted_content(true));

        let decoded = ListOpLog::load_from(&bytes).unwrap();
        if doc.oplog != decoded {
            // eprintln!("Original doc {:#?}", &doc.ops);
            // eprintln!("Loaded doc {:#?}", &decoded);
            panic!("Docs do not match!");
        }
        // assert_eq!(decoded, doc.ops);
    }
}

#[test]
#[ignore] // Removed for V3
fn encode_decode_fuzz_once() {
    fuzz_encode_decode_once(2);
}

#[test]
#[ignore]
fn encode_decode_fuzz_forever() {
    for seed in 0.. {
        if seed % 10 == 0 { println!("seed {seed}"); }
        fuzz_encode_decode_once(seed);
    }
}

fn agent_name(i: usize) -> String {
    format!("agent {}", i)
}

// This fuzzer makes 3 oplogs, and merges patches between them.
fn fuzz_encode_decode_multi(seed: u64, verbose: bool) {
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut docs = [ListCRDT::new(), ListCRDT::new(), ListCRDT::new()];

    for i in 0..docs.len() {
        // for a in 0..3 {
        //     docs[i].get_or_create_agent_id(agent_name(a).as_str());
        // }
        docs[i].get_or_create_agent_id_from_str(agent_name(i).as_str());
    }

    for _i in 0..50 {
        if verbose { println!("\n\ni {}", _i); }
        // Generate some operations
        for _j in 0..2 {
            // for _j in 0..5 {
            let idx = rng.gen_range(0..docs.len());
            let doc = &mut docs[idx];

            // make_random_change(doc, None, idx as AgentId, &mut rng);
            old_make_random_change(doc, None, 0, &mut rng, true);
        }

        let (a_idx, a, b_idx, b) = choose_2(&mut docs, &mut rng);

        // Merge by applying patches
        // let b_agent = a.get_or_create_agent_id(agent_name(b_idx).as_str());

        let encode_opts = EncodeOptions::full().store_deleted_content(true);
        let a_data = a.oplog.encode(&encode_opts);
        b.merge_data_and_ff(&a_data).unwrap();

        let b_data = b.oplog.encode(&encode_opts);
        a.merge_data_and_ff(&b_data).unwrap();

        if a.oplog != b.oplog {
            println!("Docs {} and {} after {} iterations:", a_idx, b_idx, _i);
            dbg!(&a);
            dbg!(&b);
            panic!("Documents do not match");
        } else {
            if verbose {
                println!("Merge {:?} -> '{}'", &a.oplog.cg.version, &a.branch.content);
            }
        }
    }
}


#[test]
#[ignore] // Removed for V3
fn encode_decode_multi_fuzz_once() {
    fuzz_encode_decode_multi(10, false);
}

#[test]
#[ignore]
fn encode_decode_multi_fuzz_forever() {
    for seed in 0.. {
        if seed % 20 == 0 { println!("seed {seed}"); }
        fuzz_encode_decode_multi(seed, false);
    }
}
