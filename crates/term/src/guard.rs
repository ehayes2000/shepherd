//! Keeps the parser from driving alacritty into a panic.
//!
//! alacritty_terminal 0.26 panics when a program pushes onto a full kitty keyboard stack (4096 entries): it evicts
//! the oldest entry from the title stack instead, which is usually empty. Its stacks are private, so [`Guarded`] sits
//! between the parser and the `Term`, counts each screen's kitty stack depth from what the parser dispatches, and
//! drops pushes past [`KITTY_STACK_CAP`]. This sees exactly the sequences the parser completes, so nothing is
//! filtered at the byte level. Every other call is passed through unchanged.
//!
//! The title stack (`CSI 22 t`) evicts from itself and is safe; no other alacritty stack is reachable from output.

use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

use crate::replies::Listener;

/// The most entries a kitty keyboard stack holds. A push onto a full stack is dropped, so the top stays what it
/// was. Well below alacritty's limit of 4096 (where it panics), and far above the one or two entries programs
/// push. The kitty spec evicts the oldest entry instead, but alacritty's stack can't be edited from outside.
pub const KITTY_STACK_CAP: usize = 1024;

/// The kitty keyboard stack depth of each screen. alacritty swaps the stacks along with the screens, so the active
/// screen's depth is picked by `ALT_SCREEN` at the time of each call.
#[derive(Default)]
pub(crate) struct KittyDepths {
    primary: usize,
    alt: usize,
}

impl KittyDepths {
    fn active(&mut self, mode: TermMode) -> &mut usize {
        if mode.contains(TermMode::ALT_SCREEN) {
            &mut self.alt
        } else {
            &mut self.primary
        }
    }
}

/// The `Term` as the parser's handler, with the kitty stacks kept within [`KITTY_STACK_CAP`].
pub(crate) struct Guarded<'a> {
    pub(crate) term: &'a mut Term<Listener>,
    pub(crate) kitty: &'a mut KittyDepths,
}

/// Passes each call to the `Term`'s `Handler` impl. Called by path, since `Term` has inherent methods with some of
/// the same names.
macro_rules! pass_through {
    ($(fn $name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            fn $name(&mut self, $($arg: $ty),*) {
                <Term<Listener> as Handler>::$name(self.term, $($arg),*)
            }
        )*
    };
}

impl Handler for Guarded<'_> {
    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        let depth = self.kitty.active(*self.term.mode());
        if *depth < KITTY_STACK_CAP {
            *depth += 1;
            <Term<Listener> as Handler>::push_keyboard_mode(self.term, mode);
        }
    }

    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        let depth = self.kitty.active(*self.term.mode());
        *depth = depth.saturating_sub(usize::from(to_pop));
        <Term<Listener> as Handler>::pop_keyboard_modes(self.term, to_pop);
    }

    /// RIS empties both stacks.
    fn reset_state(&mut self) {
        *self.kitty = KittyDepths::default();
        <Term<Listener> as Handler>::reset_state(self.term);
    }

    // Every other method of vte 0.15's `Handler`, so none falls back to its default no-op, except
    // `set_mouse_cursor_icon`: its type isn't exported and `Term` leaves it a no-op too. Check this list when
    // upgrading alacritty.
    pass_through! {
        fn set_title(a: Option<String>);
        fn set_cursor_style(a: Option<CursorStyle>);
        fn set_cursor_shape(a: CursorShape);
        fn input(a: char);
        fn goto(a: i32, b: usize);
        fn goto_line(a: i32);
        fn goto_col(a: usize);
        fn insert_blank(a: usize);
        fn move_up(a: usize);
        fn move_down(a: usize);
        fn identify_terminal(a: Option<char>);
        fn device_status(a: usize);
        fn move_forward(a: usize);
        fn move_backward(a: usize);
        fn move_down_and_cr(a: usize);
        fn move_up_and_cr(a: usize);
        fn put_tab(a: u16);
        fn backspace();
        fn carriage_return();
        fn linefeed();
        fn bell();
        fn substitute();
        fn newline();
        fn set_horizontal_tabstop();
        fn scroll_up(a: usize);
        fn scroll_down(a: usize);
        fn insert_blank_lines(a: usize);
        fn delete_lines(a: usize);
        fn erase_chars(a: usize);
        fn delete_chars(a: usize);
        fn move_backward_tabs(a: u16);
        fn move_forward_tabs(a: u16);
        fn save_cursor_position();
        fn restore_cursor_position();
        fn clear_line(a: LineClearMode);
        fn clear_screen(a: ClearMode);
        fn clear_tabs(a: TabulationClearMode);
        fn set_tabs(a: u16);
        fn reverse_index();
        fn terminal_attribute(a: Attr);
        fn set_mode(a: Mode);
        fn unset_mode(a: Mode);
        fn report_mode(a: Mode);
        fn set_private_mode(a: PrivateMode);
        fn unset_private_mode(a: PrivateMode);
        fn report_private_mode(a: PrivateMode);
        fn set_scrolling_region(a: usize, b: Option<usize>);
        fn set_keypad_application_mode();
        fn unset_keypad_application_mode();
        fn set_active_charset(a: CharsetIndex);
        fn configure_charset(a: CharsetIndex, b: StandardCharset);
        fn set_color(a: usize, b: Rgb);
        fn dynamic_color_sequence(a: String, b: usize, c: &str);
        fn reset_color(a: usize);
        fn clipboard_store(a: u8, b: &[u8]);
        fn clipboard_load(a: u8, b: &str);
        fn decaln();
        fn push_title();
        fn pop_title();
        fn text_area_size_pixels();
        fn text_area_size_chars();
        fn set_hyperlink(a: Option<Hyperlink>);
        fn report_keyboard_mode();
        fn set_keyboard_mode(a: KeyboardModes, b: KeyboardModesApplyBehavior);
        fn set_modify_other_keys(a: ModifyOtherKeys);
        fn report_modify_other_keys();
        fn set_scp(a: ScpCharPath, b: ScpUpdateMode);
    }
}
