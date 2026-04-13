use ratatui::style::{Color, Modifier};

#[derive(Clone)]
pub struct StyledCell {
    pub ch: char,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub modifiers: Modifier,
}

impl Default for StyledCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: None,
            bg: None,
            modifiers: Modifier::empty(),
        }
    }
}

pub struct VtermGrid {
    cells: Vec<Vec<StyledCell>>,
    cursor_row: usize,
    cursor_col: usize,
    width: u16,
    pen_fg: Option<Color>,
    pen_bg: Option<Color>,
    pen_modifiers: Modifier,
    pub alt_screen_detected: bool,
}

impl VtermGrid {
    pub fn new(width: u16) -> Self {
        Self {
            cells: vec![vec![StyledCell::default(); width as usize]],
            cursor_row: 0,
            cursor_col: 0,
            width,
            pen_fg: None,
            pen_bg: None,
            pen_modifiers: Modifier::empty(),
            alt_screen_detected: false,
        }
    }

    pub fn line_count(&self) -> usize {
        self.cells.len()
    }

    pub fn row(&self, idx: usize) -> &[StyledCell] {
        &self.cells[idx]
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut parser = vte::Parser::new();
        // FIXME: reusing a single parser across feed() calls would preserve
        // escape sequence state spanning chunk boundaries.
        for &byte in bytes {
            parser.advance(self, byte);
        }
    }

    fn ensure_row(&mut self, row: usize) {
        while self.cells.len() <= row {
            self.cells
                .push(vec![StyledCell::default(); self.width as usize]);
        }
    }

    fn put_char(&mut self, ch: char) {
        let w = self.width as usize;
        self.ensure_row(self.cursor_row);
        if self.cursor_col < w {
            self.cells[self.cursor_row][self.cursor_col] = StyledCell {
                ch,
                fg: self.pen_fg,
                bg: self.pen_bg,
                modifiers: self.pen_modifiers,
            };
            self.cursor_col += 1;
        }
    }

    fn reset_pen(&mut self) {
        self.pen_fg = None;
        self.pen_bg = None;
        self.pen_modifiers = Modifier::empty();
    }
}

impl vte::Perform for VtermGrid {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.cursor_col = 0;
                self.cursor_row += 1;
                self.ensure_row(self.cursor_row);
            },
            b'\r' => {
                self.cursor_col = 0;
            },
            b'\t' => {
                let next_tab = (self.cursor_col + 8) & !7;
                self.cursor_col = next_tab.min(self.width as usize);
            },
            0x08 => {
                self.cursor_col = self.cursor_col.saturating_sub(1);
            },
            _ => {},
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
    }
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let params_vec: Vec<u16> = params.iter().map(|p| p[0]).collect();

        if intermediates == [b'?']
            && action == 'h'
            && (params_vec.contains(&1049) || params_vec.contains(&47))
        {
            self.alt_screen_detected = true;
            return;
        }

        match action {
            'm' => {
                if params_vec.is_empty() {
                    self.reset_pen();
                    return;
                }
                let mut i = 0;
                while i < params_vec.len() {
                    match params_vec[i] {
                        0 => self.reset_pen(),
                        1 => self.pen_modifiers |= Modifier::BOLD,
                        2 => self.pen_modifiers |= Modifier::DIM,
                        3 => self.pen_modifiers |= Modifier::ITALIC,
                        4 => self.pen_modifiers |= Modifier::UNDERLINED,
                        7 => self.pen_modifiers |= Modifier::REVERSED,
                        9 => self.pen_modifiers |= Modifier::CROSSED_OUT,
                        22 => {
                            self.pen_modifiers -= Modifier::BOLD;
                            self.pen_modifiers -= Modifier::DIM;
                        },
                        23 => self.pen_modifiers -= Modifier::ITALIC,
                        24 => self.pen_modifiers -= Modifier::UNDERLINED,
                        27 => self.pen_modifiers -= Modifier::REVERSED,
                        29 => self.pen_modifiers -= Modifier::CROSSED_OUT,
                        30 => self.pen_fg = Some(Color::Black),
                        31 => self.pen_fg = Some(Color::Red),
                        32 => self.pen_fg = Some(Color::Green),
                        33 => self.pen_fg = Some(Color::Yellow),
                        34 => self.pen_fg = Some(Color::Blue),
                        35 => self.pen_fg = Some(Color::Magenta),
                        36 => self.pen_fg = Some(Color::Cyan),
                        37 => self.pen_fg = Some(Color::White),
                        38 if i + 2 < params_vec.len() && params_vec[i + 1] == 5 => {
                            self.pen_fg = Some(Color::Indexed(params_vec[i + 2] as u8));
                            i += 2;
                        },
                        38 if i + 4 < params_vec.len() && params_vec[i + 1] == 2 => {
                            self.pen_fg = Some(Color::Rgb(
                                params_vec[i + 2] as u8,
                                params_vec[i + 3] as u8,
                                params_vec[i + 4] as u8,
                            ));
                            i += 4;
                        },
                        39 => self.pen_fg = None,
                        40 => self.pen_bg = Some(Color::Black),
                        41 => self.pen_bg = Some(Color::Red),
                        42 => self.pen_bg = Some(Color::Green),
                        43 => self.pen_bg = Some(Color::Yellow),
                        44 => self.pen_bg = Some(Color::Blue),
                        45 => self.pen_bg = Some(Color::Magenta),
                        46 => self.pen_bg = Some(Color::Cyan),
                        47 => self.pen_bg = Some(Color::White),
                        48 if i + 2 < params_vec.len() && params_vec[i + 1] == 5 => {
                            self.pen_bg = Some(Color::Indexed(params_vec[i + 2] as u8));
                            i += 2;
                        },
                        48 if i + 4 < params_vec.len() && params_vec[i + 1] == 2 => {
                            self.pen_bg = Some(Color::Rgb(
                                params_vec[i + 2] as u8,
                                params_vec[i + 3] as u8,
                                params_vec[i + 4] as u8,
                            ));
                            i += 4;
                        },
                        49 => self.pen_bg = None,
                        90 => self.pen_fg = Some(Color::DarkGray),
                        91 => self.pen_fg = Some(Color::LightRed),
                        92 => self.pen_fg = Some(Color::LightGreen),
                        93 => self.pen_fg = Some(Color::LightYellow),
                        94 => self.pen_fg = Some(Color::LightBlue),
                        95 => self.pen_fg = Some(Color::LightMagenta),
                        96 => self.pen_fg = Some(Color::LightCyan),
                        97 => self.pen_fg = Some(Color::White),
                        100 => self.pen_bg = Some(Color::DarkGray),
                        101 => self.pen_bg = Some(Color::LightRed),
                        102 => self.pen_bg = Some(Color::LightGreen),
                        103 => self.pen_bg = Some(Color::LightYellow),
                        104 => self.pen_bg = Some(Color::LightBlue),
                        105 => self.pen_bg = Some(Color::LightMagenta),
                        106 => self.pen_bg = Some(Color::LightCyan),
                        107 => self.pen_bg = Some(Color::White),
                        _ => {},
                    }
                    i += 1;
                }
            },
            'A' => {
                let n = first_param(&params_vec, 1) as usize;
                self.cursor_row = self.cursor_row.saturating_sub(n);
            },
            'B' => {
                let n = first_param(&params_vec, 1) as usize;
                self.cursor_row += n;
                self.ensure_row(self.cursor_row);
            },
            'C' => {
                let n = first_param(&params_vec, 1) as usize;
                self.cursor_col = (self.cursor_col + n).min(self.width as usize - 1);
            },
            'D' => {
                let n = first_param(&params_vec, 1) as usize;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            },
            'K' => {
                let mode = first_param(&params_vec, 0);
                self.ensure_row(self.cursor_row);
                let w = self.width as usize;
                let row = &mut self.cells[self.cursor_row];
                match mode {
                    0 => {
                        for cell in row.iter_mut().take(w).skip(self.cursor_col) {
                            *cell = StyledCell::default();
                        }
                    },
                    1 => {
                        for cell in row.iter_mut().take(self.cursor_col.min(w - 1) + 1) {
                            *cell = StyledCell::default();
                        }
                    },
                    2 => {
                        for cell in row.iter_mut() {
                            *cell = StyledCell::default();
                        }
                    },
                    _ => {},
                }
            },
            'J' => {
                let mode = first_param(&params_vec, 0);
                self.ensure_row(self.cursor_row);
                let w = self.width as usize;
                match mode {
                    0 => {
                        for col in self.cursor_col..w {
                            self.cells[self.cursor_row][col] = StyledCell::default();
                        }
                        for row in (self.cursor_row + 1)..self.cells.len() {
                            for cell in &mut self.cells[row] {
                                *cell = StyledCell::default();
                            }
                        }
                    },
                    1 => {
                        for row in 0..self.cursor_row {
                            for cell in &mut self.cells[row] {
                                *cell = StyledCell::default();
                            }
                        }
                        for col in 0..=self.cursor_col.min(w - 1) {
                            self.cells[self.cursor_row][col] = StyledCell::default();
                        }
                    },
                    2 => {
                        for row in &mut self.cells {
                            for cell in row.iter_mut() {
                                *cell = StyledCell::default();
                            }
                        }
                    },
                    _ => {},
                }
            },
            _ => {},
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

fn first_param(params: &[u16], default: u16) -> u16 {
    params
        .first()
        .copied()
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

pub struct OutputBlock {
    pub command: String,
    pub grid: VtermGrid,
    pub finished: bool,
    pub exit_status: Option<i32>,
    pub error: Option<String>,
}

impl OutputBlock {
    pub fn new(command: String, width: u16) -> Self {
        Self {
            command,
            grid: VtermGrid::new(width),
            finished: false,
            exit_status: None,
            error: None,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.grid.feed(bytes);
    }
}
