// Upstream regression cases adapted from vte 0.15.0 src/lib.rs, commit
// 3b3da71c34cc1256c7e20981cf03f8eb95e08ffc. Copyright (c) 2016 Joe Wilm.
// Licensed under MIT OR Apache-2.0; full notices accompany tests/fixtures/vte.
// Only imports and fixed/no-default-feature configuration were adapted.

#[cfg(test)]
mod upstream {
    use std::vec::Vec;

    use crate::vt_parser::{Params, Parser, Perform};

    const MAX_OSC_PARAMS: usize = 16;
    const MAX_OSC_RAW: usize = 1024;
    mod params {
        pub const MAX_PARAMS: usize = 32;
    }

    const OSC_BYTES: &[u8] = &[
        0x1B, 0x5D, // Begin OSC
        b'2', b';', b'j', b'w', b'i', b'l', b'm', b'@', b'j', b'w', b'i', b'l', b'm', b'-', b'd',
        b'e', b's', b'k', b':', b' ', b'~', b'/', b'c', b'o', b'd', b'e', b'/', b'a', b'l', b'a',
        b'c', b'r', b'i', b't', b't', b'y', 0x07, // End OSC
    ];

    #[derive(Default)]
    struct Dispatcher {
        dispatched: Vec<Sequence>,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Sequence {
        Osc(Vec<Vec<u8>>, bool),
        Csi(Vec<Vec<u16>>, Vec<u8>, bool, char),
        Esc(Vec<u8>, bool, u8),
        DcsHook(Vec<Vec<u16>>, Vec<u8>, bool, char),
        DcsPut(u8),
        Print(char),
        Execute(u8),
        DcsUnhook,
    }

    impl Perform for Dispatcher {
        fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
            let params = params.iter().map(|p| p.to_vec()).collect();
            self.dispatched.push(Sequence::Osc(params, bell_terminated));
        }

        fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
            let params = params.iter().map(<[u16]>::to_vec).collect();
            let intermediates = intermediates.to_vec();
            self.dispatched
                .push(Sequence::Csi(params, intermediates, ignore, c));
        }

        fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
            let intermediates = intermediates.to_vec();
            self.dispatched
                .push(Sequence::Esc(intermediates, ignore, byte));
        }

        fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
            let params = params.iter().map(<[u16]>::to_vec).collect();
            let intermediates = intermediates.to_vec();
            self.dispatched
                .push(Sequence::DcsHook(params, intermediates, ignore, c));
        }

        fn put(&mut self, byte: u8) {
            self.dispatched.push(Sequence::DcsPut(byte));
        }

        fn unhook(&mut self) {
            self.dispatched.push(Sequence::DcsUnhook);
        }

        fn print(&mut self, c: char) {
            self.dispatched.push(Sequence::Print(c));
        }

        fn execute(&mut self, byte: u8) {
            self.dispatched.push(Sequence::Execute(byte));
        }
    }

    #[test]
    fn parse_osc() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, OSC_BYTES);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], &OSC_BYTES[2..3]);
                assert_eq!(params[1], &OSC_BYTES[4..(OSC_BYTES.len() - 1)]);
            }
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn parse_empty_osc() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &[0x1B, 0x5D, 0x07]);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(..) => (),
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn parse_osc_max_params() {
        let params = ";".repeat(params::MAX_PARAMS + 1);
        let input = format!("\x1b]{}\x1b", &params[..]).into_bytes();
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), MAX_OSC_PARAMS);
                assert!(params.iter().all(Vec::is_empty));
            }
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn osc_bell_terminated() {
        const INPUT: &[u8] = b"\x1b]11;ff/00/ff\x07";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(_, true) => (),
            _ => panic!("expected osc with bell terminator"),
        }
    }

    #[test]
    fn osc_c0_st_terminated() {
        const INPUT: &[u8] = b"\x1b]11;ff/00/ff\x1b\\";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(_, false) => (),
            _ => panic!("expected osc with ST terminator"),
        }
    }

    #[test]
    fn parse_osc_with_utf8_arguments() {
        const INPUT: &[u8] = &[
            0x0D, 0x1B, 0x5D, 0x32, 0x3B, 0x65, 0x63, 0x68, 0x6F, 0x20, 0x27, 0xC2, 0xAF, 0x5C,
            0x5F, 0x28, 0xE3, 0x83, 0x84, 0x29, 0x5F, 0x2F, 0xC2, 0xAF, 0x27, 0x20, 0x26, 0x26,
            0x20, 0x73, 0x6C, 0x65, 0x65, 0x70, 0x20, 0x31, 0x07,
        ];
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(b'\r'));
        let osc_data = INPUT[5..(INPUT.len() - 1)].into();
        assert_eq!(
            dispatcher.dispatched[1],
            Sequence::Osc(vec![vec![b'2'], osc_data], true)
        );
        assert_eq!(dispatcher.dispatched.len(), 2);
    }

    #[test]
    fn osc_containing_string_terminator() {
        const INPUT: &[u8] = b"\x1b]2;\xe6\x9c\xab\x1b\\";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params[1], &INPUT[4..(INPUT.len() - 2)]);
            }
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn exceed_max_buffer_size() {
        const NUM_BYTES: usize = MAX_OSC_RAW + 100;
        const INPUT_START: &[u8] = b"\x1b]52;s";
        const INPUT_END: &[u8] = b"\x07";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        // Create valid OSC escape
        parser.advance(&mut dispatcher, INPUT_START);

        // Exceed max buffer size
        parser.advance(&mut dispatcher, &[b'a'; NUM_BYTES]);

        // Terminate escape for dispatch
        parser.advance(&mut dispatcher, INPUT_END);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], b"52");

                assert_eq!(params[1].len(), MAX_OSC_RAW - params[0].len());
            }
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn parse_csi_max_params() {
        // This will build a list of repeating '1;'s
        // The length is MAX_PARAMS - 1 because the last semicolon is interpreted
        // as an implicit zero, making the total number of parameters MAX_PARAMS
        let params = "1;".repeat(params::MAX_PARAMS - 1);
        let input = format!("\x1b[{}p", &params[..]).into_bytes();

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(!ignore);
            }
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_ignore_long_params() {
        // This will build a list of repeating '1;'s
        // The length is MAX_PARAMS because the last semicolon is interpreted
        // as an implicit zero, making the total number of parameters MAX_PARAMS + 1
        let params = "1;".repeat(params::MAX_PARAMS);
        let input = format!("\x1b[{}p", &params[..]).into_bytes();

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(ignore);
            }
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_trailing_semicolon() {
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, b"\x1b[4;m");

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[4], [0]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_csi_params_leading_semicolon() {
        // Create dispatcher and check state
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, b"\x1b[;4m");

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[0], [4]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_long_csi_param() {
        // The important part is the parameter, which is (i64::MAX + 1)
        const INPUT: &[u8] = b"\x1b[9223372036854775808m";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, ..) => assert_eq!(params, &[[u16::MAX]]),
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn csi_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1b[?1049h";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, _) => {
                assert_eq!(intermediates, b"?");
                assert_eq!(params, &[[1049]]);
                assert!(!ignore);
            }
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn csi_subparameters() {
        const INPUT: &[u8] = b"\x1b[38:2:255:0:255;1m";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, _) => {
                assert_eq!(params, &[vec![38, 2, 255, 0, 255], vec![1]]);
                assert_eq!(intermediates.as_slice(), [] as [u8; 0]);
                assert!(!ignore);
            }
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn parse_dcs_max_params() {
        let params = "1;".repeat(params::MAX_PARAMS + 1);
        let input = format!("\x1bP{}p", &params[..]).into_bytes();
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &input);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, _, ignore, _) => {
                assert_eq!(params.len(), params::MAX_PARAMS);
                assert!(params.iter().all(|param| param == &[1]));
                assert!(ignore);
            }
            _ => panic!("expected dcs sequence"),
        }
    }

    #[test]
    fn dcs_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1bP1$tx\x9c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 3);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, intermediates, ignore, _) => {
                assert_eq!(intermediates, b"$");
                assert_eq!(params, &[[1]]);
                assert!(!ignore);
            }
            _ => panic!("expected dcs sequence"),
        }
        assert_eq!(dispatcher.dispatched[1], Sequence::DcsPut(b'x'));
        assert_eq!(dispatcher.dispatched[2], Sequence::DcsUnhook);
    }

    #[test]
    fn parse_dcs() {
        const INPUT: &[u8] = b"\x1bP0;1|17/ab\x9c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 7);
        match &dispatcher.dispatched[0] {
            Sequence::DcsHook(params, _, _, c) => {
                assert_eq!(params, &[[0], [1]]);
                assert_eq!(c, &'|');
            }
            _ => panic!("expected dcs sequence"),
        }
        for (i, byte) in b"17/ab".iter().enumerate() {
            assert_eq!(dispatcher.dispatched[1 + i], Sequence::DcsPut(*byte));
        }
        assert_eq!(dispatcher.dispatched[6], Sequence::DcsUnhook);
    }

    #[test]
    fn intermediate_reset_on_dcs_exit() {
        const INPUT: &[u8] = b"\x1bP=1sZZZ\x1b+\x5c";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 6);
        match &dispatcher.dispatched[5] {
            Sequence::Esc(intermediates, ..) => assert_eq!(intermediates, b"+"),
            _ => panic!("expected esc sequence"),
        }
    }

    #[test]
    fn esc_reset() {
        const INPUT: &[u8] = b"\x1b[3;1\x1b(A";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Esc(intermediates, ignore, byte) => {
                assert_eq!(intermediates, b"(");
                assert_eq!(*byte, b'A');
                assert!(!ignore);
            }
            _ => panic!("expected esc sequence"),
        }
    }

    #[test]
    fn esc_reset_intermediates() {
        const INPUT: &[u8] = b"\x1b[?2004l\x1b#8";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(
            dispatcher.dispatched[0],
            Sequence::Csi(vec![vec![2004]], vec![63], false, 'l')
        );
        assert_eq!(dispatcher.dispatched[1], Sequence::Esc(vec![35], false, 56));
    }

    #[test]
    fn params_buffer_filled_with_subparam() {
        const INPUT: &[u8] = b"\x1b[::::::::::::::::::::::::::::::::x\x1b";
        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, c) => {
                assert_eq!(intermediates.as_slice(), [] as [u8; 0]);
                assert_eq!(params, &[[0; 32]]);
                assert_eq!(c, &'x');
                assert!(ignore);
            }
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn build_with_fixed_size() {
        const INPUT: &[u8] = b"\x1b[3;1\x1b[?1049h";
        let mut dispatcher = Dispatcher::default();
        let mut parser: Parser<30> = Parser::new_with_size();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Csi(params, intermediates, ignore, _) => {
                assert_eq!(intermediates, b"?");
                assert_eq!(params, &[[1049]]);
                assert!(!ignore);
            }
            _ => panic!("expected csi sequence"),
        }
    }

    #[test]
    fn exceed_fixed_osc_buffer_size() {
        const OSC_BUFFER_SIZE: usize = 32;
        const NUM_BYTES: usize = OSC_BUFFER_SIZE + 100;
        const INPUT_START: &[u8] = b"\x1b]52;";
        const INPUT_END: &[u8] = b"\x07";

        let mut dispatcher = Dispatcher::default();
        let mut parser: Parser<OSC_BUFFER_SIZE> = Parser::new_with_size();

        // Create valid OSC escape
        parser.advance(&mut dispatcher, INPUT_START);

        // Exceed max buffer size
        parser.advance(&mut dispatcher, &[b'a'; NUM_BYTES]);

        // Terminate escape for dispatch
        parser.advance(&mut dispatcher, INPUT_END);

        assert_eq!(dispatcher.dispatched.len(), 1);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, _) => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], b"52");
                assert_eq!(params[1].len(), OSC_BUFFER_SIZE - params[0].len());
                for item in &params[1] {
                    assert_eq!(*item, b'a');
                }
            }
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn fixed_size_osc_containing_string_terminator() {
        const INPUT_START: &[u8] = b"\x1b]2;";
        const INPUT_MIDDLE: &[u8] = b"s\xe6\x9c\xab";
        const INPUT_END: &[u8] = b"\x1b\\";

        let mut dispatcher = Dispatcher::default();
        let mut parser: Parser<5> = Parser::new_with_size();

        parser.advance(&mut dispatcher, INPUT_START);
        parser.advance(&mut dispatcher, INPUT_MIDDLE);
        parser.advance(&mut dispatcher, INPUT_END);

        assert_eq!(dispatcher.dispatched.len(), 2);
        match &dispatcher.dispatched[0] {
            Sequence::Osc(params, false) => {
                assert_eq!(params[0], b"2");
                assert_eq!(params[1], INPUT_MIDDLE);
            }
            _ => panic!("expected osc sequence"),
        }
    }

    #[test]
    fn unicode() {
        const INPUT: &[u8] = b"\xF0\x9F\x8E\x89_\xF0\x9F\xA6\x80\xF0\x9F\xA6\x80_\xF0\x9F\x8E\x89";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 6);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('🎉'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('_'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('🦀'));
        assert_eq!(dispatcher.dispatched[3], Sequence::Print('🦀'));
        assert_eq!(dispatcher.dispatched[4], Sequence::Print('_'));
        assert_eq!(dispatcher.dispatched[5], Sequence::Print('🎉'));
    }

    #[test]
    fn invalid_utf8() {
        const INPUT: &[u8] = b"a\xEF\xBCb";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 3);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('a'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('b'));
    }

    #[test]
    fn partial_utf8() {
        const INPUT: &[u8] = b"\xF0\x9F\x9A\x80";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..2]);
        parser.advance(&mut dispatcher, &INPUT[2..3]);
        parser.advance(&mut dispatcher, &INPUT[3..]);

        assert_eq!(dispatcher.dispatched.len(), 1);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('🚀'));
    }

    #[test]
    fn partial_utf8_separating_utf8() {
        // This is different from the `partial_utf8` test since it has a multi-byte UTF8
        // character after the partial UTF8 state, causing a partial byte to be present
        // in the `partial_utf8` buffer after the 2-byte codepoint.

        // "ĸ🎉"
        const INPUT: &[u8] = b"\xC4\xB8\xF0\x9F\x8E\x89";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..]);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('ĸ'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('🎉'));
    }

    #[test]
    fn partial_invalid_utf8() {
        const INPUT: &[u8] = b"a\xEF\xBCb";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..1]);
        parser.advance(&mut dispatcher, &INPUT[1..2]);
        parser.advance(&mut dispatcher, &INPUT[2..3]);
        parser.advance(&mut dispatcher, &INPUT[3..]);

        assert_eq!(dispatcher.dispatched.len(), 3);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('a'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('b'));
    }

    #[test]
    fn partial_invalid_utf8_split() {
        const INPUT: &[u8] = b"\xE4\xBF\x99\xB5";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, &INPUT[..2]);
        parser.advance(&mut dispatcher, &INPUT[2..]);

        assert_eq!(dispatcher.dispatched[0], Sequence::Print('俙'));
        assert_eq!(dispatcher.dispatched[1], Sequence::Print('�'));
    }

    #[test]
    fn partial_utf8_into_esc() {
        const INPUT: &[u8] = b"\xD8\x1b012";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 4);
        assert_eq!(dispatcher.dispatched[0], Sequence::Print('�'));
        assert_eq!(
            dispatcher.dispatched[1],
            Sequence::Esc(Vec::new(), false, b'0')
        );
        assert_eq!(dispatcher.dispatched[2], Sequence::Print('1'));
        assert_eq!(dispatcher.dispatched[3], Sequence::Print('2'));
    }

    #[test]
    fn c1s() {
        const INPUT: &[u8] = b"\x00\x1f\x80\x90\x98\x9b\x9c\x9d\x9e\x9fa";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 11);
        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(0));
        assert_eq!(dispatcher.dispatched[1], Sequence::Execute(31));
        assert_eq!(dispatcher.dispatched[2], Sequence::Execute(128));
        assert_eq!(dispatcher.dispatched[3], Sequence::Execute(144));
        assert_eq!(dispatcher.dispatched[4], Sequence::Execute(152));
        assert_eq!(dispatcher.dispatched[5], Sequence::Execute(155));
        assert_eq!(dispatcher.dispatched[6], Sequence::Execute(156));
        assert_eq!(dispatcher.dispatched[7], Sequence::Execute(157));
        assert_eq!(dispatcher.dispatched[8], Sequence::Execute(158));
        assert_eq!(dispatcher.dispatched[9], Sequence::Execute(159));
        assert_eq!(dispatcher.dispatched[10], Sequence::Print('a'));
    }

    #[test]
    fn execute_anywhere() {
        const INPUT: &[u8] = b"\x18\x1a";

        let mut dispatcher = Dispatcher::default();
        let mut parser = Parser::new();

        parser.advance(&mut dispatcher, INPUT);

        assert_eq!(dispatcher.dispatched.len(), 2);
        assert_eq!(dispatcher.dispatched[0], Sequence::Execute(0x18));
        assert_eq!(dispatcher.dispatched[1], Sequence::Execute(0x1A));
    }
}

use crate::vt_parser::{Params, Parser, Perform};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Recorder {
    pub(crate) dispatched: Vec<Event>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Osc(Vec<Vec<u8>>, bool),
    Csi(Vec<Vec<u16>>, Vec<u8>, bool, char),
    Esc(Vec<u8>, bool, u8),
    DcsHook(Vec<Vec<u16>>, Vec<u8>, bool, char),
    DcsPut(u8),
    Print(char),
    Execute(u8),
    DcsUnhook,
}

impl Perform for Recorder {
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        let params = params.iter().map(|p| p.to_vec()).collect();
        self.dispatched.push(Event::Osc(params, bell_terminated));
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
        let params = params.iter().map(<[u16]>::to_vec).collect();
        let intermediates = intermediates.to_vec();
        self.dispatched
            .push(Event::Csi(params, intermediates, ignore, c));
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        let intermediates = intermediates.to_vec();
        self.dispatched
            .push(Event::Esc(intermediates, ignore, byte));
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, c: char) {
        let params = params.iter().map(<[u16]>::to_vec).collect();
        let intermediates = intermediates.to_vec();
        self.dispatched
            .push(Event::DcsHook(params, intermediates, ignore, c));
    }

    fn put(&mut self, byte: u8) {
        self.dispatched.push(Event::DcsPut(byte));
    }

    fn unhook(&mut self) {
        self.dispatched.push(Event::DcsUnhook);
    }

    fn print(&mut self, c: char) {
        self.dispatched.push(Event::Print(c));
    }

    fn execute(&mut self, byte: u8) {
        self.dispatched.push(Event::Execute(byte));
    }
}

// Independent deterministic corpus extensions (no external parser dependency).

pub(crate) const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/vte/demo.vte");

pub(crate) const PREFIXES: &[&[u8]] = &[
    b"",
    b"\x1b",
    b"\x1b ",
    b"\x1b  ",
    b"\x1b   ",
    b"\x1b[",
    b"\x1b[?",
    b"\x1b[1;",
    b"\x1b[1:",
    b"\x1b[1 ",
    b"\x1b[1?",
    b"\x1b[   ",
    b"\x1bP",
    b"\x1bP?",
    b"\x1bP1;",
    b"\x1bP1:",
    b"\x1bP1 ",
    b"\x1bP1?",
    b"\x1bP   ",
    b"\x1bPq",
    b"\x1bPqabc",
    b"\x1b]",
    b"\x1b]2;",
    b"\x1b]2;abc\x1b",
    b"\x1bX",
    b"\x1b^",
    b"\x1b_",
    b"\xc2",
    b"\xe2",
    b"\xe2\x82",
    b"\xf0",
    b"\xf0\x9f",
    b"\xf0\x9f\x92",
];

pub(crate) fn trace(bytes: &[u8], chunk: usize) -> Vec<Event> {
    let mut parser = Parser::new();
    let mut recorder = Recorder::default();
    parser.advance(&mut recorder, b"");
    for bytes in bytes.chunks(chunk.max(1)) {
        parser.advance(&mut recorder, bytes);
        parser.advance(&mut recorder, b"");
    }
    recorder.dispatched
}

fn every_split(bytes: &[u8]) {
    let expected = trace(bytes, bytes.len());
    assert_eq!(trace(bytes, 1), expected, "bytewise {bytes:02x?}");
    for split in 0..=bytes.len() {
        let mut parser = Parser::new();
        let mut recorder = Recorder::default();
        parser.advance(&mut recorder, &bytes[..split]);
        parser.advance(&mut recorder, b"");
        parser.advance(&mut recorder, &bytes[split..]);
        assert_eq!(recorder.dispatched, expected, "split {split}: {bytes:02x?}");
    }
}

#[test]
fn all_short_partitions_with_empty_chunks() {
    for bytes in [
        b"\x1b[1:2m".as_slice(),
        b"\x1b]a;\x07",
        b"\x1bPq!\x1b\\",
        b"\xe2\x82\xac",
        b"\xe2!\x1b[0m",
    ] {
        let expected = trace(bytes, bytes.len());
        for mask in 0..(1usize << bytes.len().saturating_sub(1)) {
            let mut parser = Parser::new();
            let mut recorder = Recorder::default();
            let mut start = 0;
            for end in 1..=bytes.len() {
                if end == bytes.len() || mask & (1 << (end - 1)) != 0 {
                    parser.advance(&mut recorder, b"");
                    parser.advance(&mut recorder, &bytes[start..end]);
                    parser.advance(&mut recorder, b"");
                    start = end;
                }
            }
            assert_eq!(
                recorder.dispatched, expected,
                "partition {mask}: {bytes:02x?}"
            );
        }
    }
}

pub(crate) fn matrix_golden_text(
    prefix_index: usize,
    mut parse: impl FnMut(&[u8], &[usize]) -> Vec<Event>,
) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for byte in 0..=u8::MAX {
        let mut input = PREFIXES[prefix_index].to_vec();
        input.push(byte);
        input.extend_from_slice(b"\x18\x1b[0mOK\x1b]2;x\x07");
        let bytewise: Vec<usize> = (0..=input.len()).collect();
        writeln!(
            &mut text,
            "prefix {prefix_index}, input {input:02x?}, bytewise"
        )
        .unwrap();
        text.push_str(&formatted_events(&parse(&input, &bytewise)));
        for split in 0..=input.len() {
            writeln!(
                &mut text,
                "prefix {prefix_index}, input {input:02x?}, split {split}"
            )
            .unwrap();
            text.push_str(&formatted_events(&parse(&input, &[0, split, input.len()])));
        }
    }
    text
}

fn trace_partition(input: &[u8], boundaries: &[usize]) -> Vec<Event> {
    let mut parser = Parser::new();
    let mut recorder = Recorder::default();
    parser.advance(&mut recorder, b"");
    for pair in boundaries.windows(2) {
        parser.advance(&mut recorder, &input[pair[0]..pair[1]]);
        parser.advance(&mut recorder, b"");
    }
    recorder.dispatched
}

#[test]
fn every_state_prefix_and_next_byte_matches_schedule_golden() {
    for prefix in 0..PREFIXES.len() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/vte/matrix.prefix-{prefix}.trace"));
        let expected = read_compact_golden(&path)
            .expect("capture pinned upstream matrix schedule traces first");
        assert_golden_text(
            &matrix_golden_text(prefix, trace_partition),
            &expected,
            &format!("matrix prefix {prefix}"),
        );
    }
}

#[test]
fn malformed_utf8_and_recovery_chunk_invariance() {
    for malformed in [
        b"\xc0\xaf".as_slice(),
        b"\xc1\xbf",
        b"\xed\xa0\x80",
        b"\xf4\x90\x80\x80",
        b"\xf5\x80\x80\x80",
        b"\xff\xfe\x80",
        b"\xe0\x80\x80",
        b"\xf0\x80\x80\x80",
        b"\xe2\x1b",
        b"\xf0\x9f\x18",
        b"\xe2\x82!",
        b"\xc2\xc2\xa3",
    ] {
        let mut input = malformed.to_vec();
        input.extend_from_slice(b"\x18\x1b[0mZ");
        every_split(&input);
        assert_eq!(trace(&input, 1).last(), Some(&Event::Print('Z')));
    }
}

#[test]
fn cancellation_and_recovery_preserve_order() {
    for prefix in PREFIXES.iter().filter(|prefix| prefix.starts_with(b"\x1b")) {
        for cancel in [0x18, 0x1a] {
            let mut input = prefix.to_vec();
            input.extend_from_slice(&[cancel, b'Z']);
            every_split(&input);
            let events = trace(&input, 1);
            assert_eq!(events.last(), Some(&Event::Print('Z')), "{input:02x?}");
            assert!(events.contains(&Event::Execute(cancel)), "{input:02x?}");
        }
    }
}

pub(crate) fn boundary_inputs() -> Vec<Vec<u8>> {
    let mut inputs = Vec::new();
    for count in [0, 1, 2, 3, 15, 16, 17, 31, 32, 33, 63, 64, 65] {
        for separator in [';', ':'] {
            for start in ["\x1b[", "\x1bP"] {
                inputs.push(
                    format!("{start}{}1mTAIL\x18", format!("1{separator}").repeat(count))
                        .into_bytes(),
                );
            }
        }
        inputs.push(format!("\x1b]{}z\x07Z", "a;".repeat(count)).into_bytes());
        inputs.push(format!("\x1b[{}mZ", " ".repeat(count)).into_bytes());
        inputs.push(format!("\x1bP{}qdata\x1b\\Z", " ".repeat(count)).into_bytes());
    }
    for count in [0, 1, 7, 8, 9, 1023, 1024, 1025, 2048] {
        inputs.push(format!("\x1b]2;{}\x07Z", "a".repeat(count)).into_bytes());
    }
    for value in [
        "65534",
        "65535",
        "65536",
        "99999999999999999999999999999999999",
    ] {
        inputs.push(format!("\x1b[{value};1mZ").into_bytes());
    }
    inputs
}

#[test]
fn resource_boundaries_keep_bounded_callbacks_and_recover() {
    for input in boundary_inputs() {
        for chunk in [1, 2, 3, 7, 31, 1024] {
            let events = trace(&input, chunk);
            assert_eq!(
                events,
                trace(&input, input.len()),
                "chunk {chunk}: {input:02x?}"
            );
            for event in &events {
                match event {
                    Event::Csi(params, intermediates, _, _)
                    | Event::DcsHook(params, intermediates, _, _) => {
                        assert!(params.iter().map(Vec::len).sum::<usize>() <= 32);
                        assert!(intermediates.len() <= 2);
                    }
                    Event::Osc(params, _) => {
                        assert!(params.len() <= 16);
                        assert!(params.iter().map(Vec::len).sum::<usize>() <= 1024);
                    }
                    _ => {}
                }
            }
            assert!(events.contains(&Event::Print('Z')) || input.ends_with(b"\x18"));
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Generator(pub(crate) u64);
impl Generator {
    pub(crate) fn next(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.to_le_bytes()[0]
    }
    pub(crate) fn input(&mut self, grammar: bool) -> Vec<u8> {
        const ATOMS: &[&[u8]] = &[
            b"text",
            "π🙂".as_bytes(),
            b"\x1b[0m",
            b"\x1b[38:2::12:34:56m",
            b"\x1b[?25h",
            b"\x1b[65536;1m",
            b"\x1b]2;title\x07",
            b"\x1b]4;1;rgb:f/e/d\x1b\\",
            b"\x1bP1;2$qpayload\x1b\\",
            b"\x1b(0",
            b"\x18",
            b"\x1a",
            b"\x1b_ignored\x1b\\",
            b"\xe2\x82",
            b"\xed\xa0\x80",
            b"\x00\x07\x0d\x0a",
            b"\x1b[1:",
            b"\x1b]",
        ];
        let count = usize::from(self.next()) + 1;
        let mut input = Vec::new();
        for _ in 0..count {
            if grammar {
                input.extend_from_slice(ATOMS[usize::from(self.next()) % ATOMS.len()]);
            } else {
                input.push(self.next());
            }
        }
        input.extend_from_slice(b"\x18\x1b[0mEND");
        input
    }
}

#[test]
fn deterministic_generated_chunk_properties() {
    for seed in [1, 0x5eed_cafe, 0x0123_4567_89ab_cdef, u64::MAX] {
        for grammar in [false, true] {
            let mut generator = Generator(seed);
            for case in 0..128 {
                let input = generator.input(grammar);
                // Arbitrary/malformed inputs may legitimately be chunk-sensitive
                // in the pinned baseline. Check deterministic same-schedule replay
                // and recovery here; differential tests establish exact semantics.
                for chunk in [1, 2, 3, 7, 16, 31, 127] {
                    let actual = trace(&input, chunk);
                    assert!(
                        actual == trace(&input, chunk),
                        "non-deterministic seed {seed:x}, grammar {grammar}, case {case}, chunk {chunk}: {input:02x?}"
                    );
                    assert_eq!(actual.last(), Some(&Event::Print('D')));
                    for event in actual {
                        match event {
                            Event::Csi(params, intermediates, _, _)
                            | Event::DcsHook(params, intermediates, _, _) => {
                                assert!(params.iter().map(Vec::len).sum::<usize>() <= 32);
                                assert!(intermediates.len() <= 2);
                            }
                            Event::Osc(params, _) => {
                                assert!(params.len() <= 16);
                                assert!(params.iter().map(Vec::len).sum::<usize>() <= 1024);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}

pub(crate) const FIXTURE_CHUNKS: &[usize] = &[1, 2, 3, 7, 16, 31, 127, 1024, 13809];

#[test]
fn fixture_chunk_properties() {
    for &chunk in FIXTURE_CHUNKS {
        assert_golden_events(&format!("demo.chunk-{chunk}.trace"), &trace(FIXTURE, chunk));
    }
}

// ANSI Processor regressions: syntax preserved here, semantics owned by consumers.
pub(crate) const ANSI_INPUTS: &[(&str, &[u8])] = &[
    ("parse_control_attribute", b"\x1b[1m"),
    ("parse_terminal_identity_csi", b"\x1b[1c\x1b[c\x1b[0c"),
    ("parse_terminal_identity_esc", b"\x1bZ"),
    ("parse_truecolor_attr", b"\x1b[38;2;128;66;255m"),
    ("parse_designate_g0_as_line_drawing", b"\x1b(0"),
    (
        "parse_designate_g1_as_line_drawing_and_invoke",
        b"\x1b)0\x0e",
    ),
    ("parse_osc4_set_color", b"\x1b]4;0;#fff\x1b\\"),
    ("parse_osc104_reset_color", b"\x1b]104;1;\x1b\\"),
    ("parse_osc104_reset_all_colors", b"\x1b]104;\x1b\\"),
    (
        "parse_osc104_reset_all_colors_no_semicolon",
        b"\x1b]104\x1b\\",
    ),
    (
        "partial_sync_updates",
        b"\x1b[?2026hrandom \x1b[31m stuff\x1b[?2026h\x1b[?2026l",
    ),
    ("mixed_sync_escape", b"\x1b[?2026h\x1b[31m\x1b[?2026l"),
    (
        "sync_bsu_with_esu",
        b"\x1b[?2026h\x1b[1m\x1b[?2026l\x1b[?2026h\x1b[4m\x1b[?2026l",
    ),
    (
        "parse_zsh_startup",
        &[
            0x1B, b'[', b'1', b'm', 0x1B, b'[', b'7', b'm', b'%', 0x1B, b'[', b'2', b'7', b'm',
            0x1B, b'[', b'1', b'm', 0x1B, b'[', b'0', b'm', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ', b' ',
            b' ', b' ', b' ', b'\r', b' ', b'\r', b'\r', 0x1B, b'[', b'0', b'm', 0x1B, b'[', b'2',
            b'7', b'm', 0x1B, b'[', b'2', b'4', b'm', 0x1B, b'[', b'J', b'j', b'w', b'i', b'l',
            b'm', b'@', b'j', b'w', b'i', b'l', b'm', b'-', b'd', b'e', b's', b'k', b' ', 0x1B,
            b'[', b'0', b'1', b';', b'3', b'2', b'm', 0xE2, 0x9E, 0x9C, b' ', 0x1B, b'[', b'0',
            b'1', b';', b'3', b'2', b'm', b' ', 0x1B, b'[', b'3', b'6', b'm', b'~', b'/', b'c',
            b'o', b'd', b'e',
        ],
    ),
];

#[test]
fn ansi_processor_input_trace_equivalents() {
    for (name, input) in ANSI_INPUTS {
        every_split(input);
        assert!(!trace(input, 1).is_empty(), "{name}");
    }
    assert_eq!(
        trace(b"\x1b[1m", 1),
        vec![Event::Csi(vec![vec![1]], vec![], false, 'm')]
    );
    assert_eq!(
        trace(b"\x1b[1c\x1b[c\x1b[0c", 1),
        vec![
            Event::Csi(vec![vec![1]], vec![], false, 'c'),
            Event::Csi(vec![vec![0]], vec![], false, 'c'),
            Event::Csi(vec![vec![0]], vec![], false, 'c'),
        ]
    );
    assert_eq!(trace(b"\x1bZ", 1), vec![Event::Esc(vec![], false, b'Z')]);
    assert_eq!(
        trace(b"\x1b[38;2;128;66;255m", 1),
        vec![Event::Csi(
            vec![vec![38], vec![2], vec![128], vec![66], vec![255]],
            vec![],
            false,
            'm'
        )]
    );
    assert_eq!(
        trace(b"\x1b(0", 1),
        vec![Event::Esc(vec![b'('], false, b'0')]
    );
    assert_eq!(
        trace(b"\x1b)0\x0e", 1),
        vec![Event::Esc(vec![b')'], false, b'0'), Event::Execute(14)]
    );
    for payload in [b"4;0;#fff".as_slice(), b"104;1;", b"104;", b"104"] {
        let mut input = b"\x1b]".to_vec();
        input.extend_from_slice(payload);
        input.extend_from_slice(b"\x1b\\");
        assert_eq!(
            trace(&input, 1),
            vec![
                Event::Osc(
                    payload
                        .split(|byte| *byte == b';')
                        .map(<[u8]>::to_vec)
                        .collect(),
                    false
                ),
                Event::Esc(vec![], false, b'\\')
            ]
        );
    }
    for color in [
        "rgb:f/e/d",
        "rgb:11/aa/ff",
        "rgb:f/ed1/cb23",
        "rgb:ffff/0/0",
        "#1af",
        "#11aaff",
        "#110aa0ff0",
        "#1100aa00ff00",
        "rgb:0//",
        "rgb://///",
        "#",
        "#f",
    ] {
        let input = format!("\x1b]4;1;{color}\x07");
        assert_eq!(
            trace(input.as_bytes(), 1),
            vec![Event::Osc(
                vec![b"4".to_vec(), b"1".to_vec(), color.as_bytes().to_vec()],
                true
            )]
        );
        every_split(input.as_bytes());
    }
    for number in ["1abc", "123", "321"] {
        let input = format!("\x1b]104;{number}\x07");
        assert_eq!(
            trace(input.as_bytes(), 1),
            vec![Event::Osc(
                vec![b"104".to_vec(), number.as_bytes().to_vec()],
                true
            )]
        );
    }
    // Processor synchronization buffer policy is not part of the byte parser.
    // A bounded payload still verifies parser order across a large text burst.
    let input = format!("\x1b[?2026h\x1b[31m{}\x1b[31m", "a".repeat(65536));
    let expected = trace(input.as_bytes(), input.len());
    for chunk in [1, 7, 1024] {
        assert_eq!(trace(input.as_bytes(), chunk), expected);
    }
    assert_eq!(
        expected
            .iter()
            .filter(|event| **event == Event::Print('a'))
            .count(),
        65536
    );
}

pub(crate) fn formatted_events(events: &[Event]) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for event in events {
        writeln!(&mut text, "{event:?}").unwrap();
    }
    text
}

fn assert_golden_events(name: &str, events: &[Event]) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/vte")
        .join(name);
    let expected = read_compact_golden(&path)
        .expect("capture pinned upstream schedule-specific golden traces first");
    assert_golden_text(&formatted_events(events), &expected, name);
}

fn assert_golden_text(actual: &str, expected: &str, context: &str) {
    if actual == expected {
        return;
    }
    let actual_lines: Vec<_> = actual.lines().collect();
    let expected_lines: Vec<_> = expected.lines().collect();
    let first = actual_lines
        .iter()
        .zip(&expected_lines)
        .position(|(a, b)| a != b)
        .unwrap_or(actual_lines.len().min(expected_lines.len()));
    let preview = |lines: &[&str]| -> String {
        let start = first.saturating_sub(1).min(lines.len());
        let end = first.saturating_add(2).min(lines.len());
        lines[start..end]
            .iter()
            .map(|line| line.chars().take(512).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    panic!(
        "{context}: first differing line {first}; actual/expected lines {}/{}; actual/expected bytes {}/{}; actual window:\n{}\nexpected window:\n{}",
        actual_lines.len(),
        expected_lines.len(),
        actual.len(),
        expected.len(),
        preview(&actual_lines),
        preview(&expected_lines)
    );
}

pub(crate) fn generated_golden_text(mut parse: impl FnMut(&[u8], usize) -> Vec<Event>) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for seed in [1, 0x5eed_cafe, 0x0123_4567_89ab_cdef, u64::MAX] {
        for grammar in [false, true] {
            let mut generator = Generator(seed);
            for case in 0..128 {
                let input = generator.input(grammar);
                for chunk in [1, 2, 3, 7, 16, 31, 127, input.len()] {
                    writeln!(&mut text, "seed {seed:x}, grammar {grammar}, case {case}, chunk {chunk}, input {input:02x?}").unwrap();
                    text.push_str(&formatted_events(&parse(&input, chunk)));
                }
            }
        }
    }
    text
}

#[test]
fn generated_matches_retained_same_schedule_golden_traces() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vte/generated.trace");
    let expected = read_compact_golden(&path)
        .expect("capture pinned upstream generated schedule traces first");
    assert_golden_text(
        &generated_golden_text(trace),
        &expected,
        "generated same-schedule golden",
    );
}

#[test]
fn fixture_matches_retained_ordered_golden_trace() {
    assert_golden_events("demo.chunk-13809.trace", &trace(FIXTURE, FIXTURE.len()));
}

#[test]
fn decoded_c1_whole_and_split_schedule_regressions() {
    // Pinned upstream ground_dispatch executes decoded C1, whereas completion
    // in advance_partial_utf8 prints it. Preserve both exact schedule meanings.
    for byte in 0x80..=0x9fu8 {
        let input = [0xc2, byte];
        assert_eq!(trace(&input, 2), vec![Event::Execute(byte)]);
        assert_eq!(trace(&input, 1), vec![Event::Print(char::from(byte))]);
    }
}

// Malformed byte cases saved independently of the generated corpus. Expected
// callbacks are captured per schedule from pinned upstream, never normalized.
pub(crate) const SAVED_MALFORMED: &[&[u8]] = &[
    b"\xc2\x80",
    b"\xc2\x9f",
    b"\xc2!",
    b"\xe2\x82!",
    b"\xed\xa0\x80",
    b"\xf4\x90\x80\x80",
    b"\xe2\x1b[0mZ",
];

pub(crate) fn malformed_golden_text(mut parse: impl FnMut(&[u8], usize) -> Vec<Event>) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for input in SAVED_MALFORMED {
        for chunk in [1, 2, 3, input.len()] {
            writeln!(&mut text, "input {input:02x?}, chunk {chunk}").unwrap();
            text.push_str(&formatted_events(&parse(input, chunk)));
        }
    }
    text
}

#[test]
fn saved_malformed_matches_retained_same_schedule_golden_traces() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/vte/malformed.trace");
    let expected = read_compact_golden(&path)
        .expect("capture pinned upstream malformed schedule traces first");
    assert_golden_text(
        &malformed_golden_text(trace),
        &expected,
        "saved malformed schedules",
    );
}

fn read_compact_golden(path: &std::path::Path) -> Result<String, String> {
    let dictionary =
        std::fs::read_to_string(path.with_extension("dict")).map_err(|error| error.to_string())?;
    let entries = decode_compact_dictionary(&dictionary)?;
    let encoded = std::fs::read(path.with_extension("idx")).map_err(|e| e.to_string())?;
    if encoded.get(..8) != Some(b"VTIDX2\0\0") {
        return Err("invalid index version".into());
    }
    let integer = |start: usize, width: usize| -> Result<usize, String> {
        let bytes = encoded
            .get(start..start + width)
            .ok_or("truncated integer")?;
        let mut value = [0u8; 8];
        value[..width].copy_from_slice(bytes);
        usize::try_from(u64::from_le_bytes(value)).map_err(|e| e.to_string())
    };
    let count = integer(8, 8)?;
    let size = integer(16, 8)?;
    if count > 4_000_000 || size > 100_000_000 {
        return Err("golden resource limit".into());
    }
    let mut ids = Vec::with_capacity(count);
    let mut offset = 24;
    while offset < encoded.len() {
        let tag = encoded[offset];
        offset += 1;
        match tag {
            0 => {
                let index = compact_varint(&encoded, &mut offset)?;
                if index >= entries.len() || ids.len() >= count {
                    return Err("invalid literal index/count".into());
                }
                ids.push(index);
            }
            1 => {
                let distance = compact_varint(&encoded, &mut offset)?;
                let length = compact_varint(&encoded, &mut offset)?;
                if distance == 0
                    || distance > ids.len()
                    || length == 0
                    || ids.len().checked_add(length).is_none_or(|n| n > count)
                {
                    return Err("invalid index backreference".into());
                }
                for _ in 0..length {
                    ids.push(ids[ids.len() - distance]);
                }
            }
            _ => return Err("unknown index tag".into()),
        }
    }
    if ids.len() != count {
        return Err("decoded line count mismatch".into());
    }
    let mut result = String::with_capacity(size);
    for index in ids {
        if result
            .len()
            .checked_add(entries[index].len())
            .is_none_or(|n| n > size)
        {
            return Err("decoded byte length overflow".into());
        }
        result.push_str(&entries[index]);
    }
    if result.len() != size {
        return Err("decoded byte length mismatch".into());
    }
    Ok(result)
}

fn decode_compact_dictionary(dictionary: &str) -> Result<Vec<String>, String> {
    let mut lines = dictionary.lines();
    if lines.next() != Some("VTDICT2") {
        return Err("invalid dictionary version".into());
    }
    let mut entries = Vec::new();
    let mut previous = String::new();
    for line in lines {
        let mut fields = line.splitn(3, '\t');
        let prefix = fields
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or("invalid prefix length")?;
        let suffix = fields
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .ok_or("invalid suffix length")?;
        let middle = fields.next().ok_or("missing dictionary text")?;
        if prefix
            .checked_add(suffix)
            .is_none_or(|n| n > previous.len())
        {
            return Err("overlapping dictionary affixes".into());
        }
        let tail_start = previous.len() - suffix;
        let head = previous
            .get(..prefix)
            .ok_or("invalid UTF-8 prefix boundary")?;
        let tail = previous
            .get(tail_start..)
            .ok_or("invalid UTF-8 suffix boundary")?;
        let next = format!("{head}{middle}{tail}");
        entries.push(format!("{next}\n"));
        previous = next;
    }
    Ok(entries)
}

fn compact_varint(encoded: &[u8], offset: &mut usize) -> Result<usize, String> {
    let mut value = 0u32;
    for shift in [0, 7, 14, 21, 28] {
        let byte = *encoded.get(*offset).ok_or("truncated varint")?;
        *offset += 1;
        if shift == 28 && byte > 15 {
            return Err("varint overflow".into());
        }
        value |= u32::from(byte & 127) << shift;
        if byte < 128 {
            return usize::try_from(value).map_err(|e| e.to_string());
        }
    }
    Err("unterminated varint".into())
}
