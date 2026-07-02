//! Integration layer between the terminal UI and the chess engine.
//!
//! [`GameController`] owns the game state (a [`Board`] plus the played move
//! list) and drives the various game modes. It records moves in SAN, runs real
//! searches for engine moves / hints / analysis, evaluates positions with the
//! real evaluator, detects game end through [`game_status`], and saves/loads
//! games as PGN.

use crate::board::Board;
use crate::engine::{game_status, GameStatus};
use crate::eval::eval_terms;
use crate::log::SearchTelemetry;
use crate::moves::Move;
use crate::movegen::legal_moves;
use crate::pgn::PgnGame;
use crate::san::move_to_san;
use crate::search::{search, SearchLimits};
use crate::types::{square, Color, Piece, Sq};
use crate::ui::{
    AsciiArt, ConfirmDialog, GameInterface, GameMode, GameResult, GameSettings, InputValidator,
    MoveHistoryDisplay, Notification, NotificationKind, StatsDisplay, create_game_mode_menu,
    create_main_menu,
};
use std::io;
use std::time::Instant;

/// Orchestrates a game: state, history, settings, and the main input loops.
pub struct GameController {
    board: Board,
    interface: GameInterface,
    settings: GameSettings,
    move_history: MoveHistoryDisplay,
    /// The mainline moves actually played (source of truth for SAN/PGN).
    played: Vec<Move>,
    /// Moves that were undone and can be redone.
    redo: Vec<Move>,
    /// FEN the current game started from (for PGN export).
    start_fen: String,
    game_active: bool,
    /// Telemetry of the most recent search (for the `stats` command).
    last_search: Option<SearchTelemetry>,
}

/// What became of a command string offered to [`GameController::shared_command`].
enum Shared {
    /// Not a shared command; the caller should try its own handling.
    NotHandled,
    /// Executed; the position or board display changed, so redraw.
    Redraw,
    /// Executed; nothing on the board changed.
    Stay,
}

impl GameController {
    pub fn new() -> Self {
        Self {
            board: Board::startpos(),
            interface: GameInterface::new(),
            settings: GameSettings::default(),
            move_history: MoveHistoryDisplay::new(),
            played: Vec::new(),
            redo: Vec::new(),
            start_fen: crate::board::START_FEN.to_string(),
            game_active: false,
            last_search: None,
        }
    }

    pub fn run(&mut self) {
        AsciiArt::show_welcome_banner();

        loop {
            let menu = create_main_menu();
            menu.display();

            match menu.get_selection() {
                Ok(action) => match action.as_str() {
                    "new_game" => self.start_new_game(),
                    "load_game" => self.load_game(),
                    "settings" => self.configure_settings(),
                    "tutorial" => self.show_tutorial(),
                    "stats" => self.show_statistics(),
                    "about" => self.show_about(),
                    "logout"
                        if ConfirmDialog::confirm("Are you sure you want to logout?") => {
                            break;
                        }
                    "exit"
                        if ConfirmDialog::confirm("Are you sure you want to exit?") => {
                            std::process::exit(0);
                        }
                    _ => {}
                },
                Err(e) => eprintln!("Error: {}", e),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Move application (keeps board, history, and PGN list in lock-step)
    // -----------------------------------------------------------------------

    /// Commit a legal move: SAN is computed *before* the position changes, then
    /// the board, the played-move list, and the display history are updated.
    fn commit(&mut self, mv: Move) {
        let san = move_to_san(&self.board, mv);
        self.board.make_move_struct(mv);
        self.played.push(mv);
        self.redo.clear();
        self.interface.highlight_move(mv.from, mv.to);
        self.move_history.add_move(san.clone());
        self.interface.add_move_to_history(san);
    }

    fn undo(&mut self) {
        if let Some(mv) = self.played.pop() {
            self.board.unmake_move();
            self.redo.push(mv);
            self.rebuild_history();
            match self.played.last() {
                Some(last) => self.interface.highlight_move(last.from, last.to),
                None => self.interface.clear_highlights(),
            }
        }
    }

    fn redo_last(&mut self) {
        if let Some(mv) = self.redo.pop() {
            let san = move_to_san(&self.board, mv);
            self.board.make_move_struct(mv);
            self.played.push(mv);
            self.interface.highlight_move(mv.from, mv.to);
            self.move_history.add_move(san.clone());
            self.interface.add_move_to_history(san);
        }
    }

    /// Recompute the SAN move-history display by replaying the played moves on
    /// a fresh board (used after an undo).
    fn rebuild_history(&mut self) {
        self.move_history.clear();
        self.interface.clear_history();
        let mut b = Board::from_fen(&self.start_fen).unwrap_or_else(|_| Board::startpos());
        for &mv in &self.played {
            let san = move_to_san(&b, mv);
            b.make_move_struct(mv);
            self.move_history.add_move(san.clone());
            self.interface.add_move_to_history(san);
        }
    }

    // -----------------------------------------------------------------------
    // Game setup & loops
    // -----------------------------------------------------------------------

    fn start_new_game(&mut self) {
        let mode_menu = create_game_mode_menu();
        mode_menu.display();

        let mode = match mode_menu.get_selection() {
            Ok(action) => match action.as_str() {
                "human_vs_engine" => GameMode::HumanVsEngine,
                "human_vs_human" => GameMode::HumanVsHuman,
                "engine_vs_engine" => GameMode::EngineVsEngine,
                "analysis" => GameMode::Analysis,
                _ => return,
            },
            Err(_) => return,
        };

        self.interface.set_game_mode(mode);
        self.reset_game();

        Notification::new(
            "Game started! Good luck!".to_string(),
            NotificationKind::Success,
        )
        .show_timed(1200);

        match mode {
            GameMode::HumanVsEngine => self.play_human_vs_engine(),
            GameMode::HumanVsHuman => self.play_human_vs_human(),
            GameMode::EngineVsEngine => self.play_engine_vs_engine(),
            GameMode::Analysis => self.analysis_mode(),
        }
    }

    fn reset_game(&mut self) {
        self.board = Board::startpos();
        self.start_fen = crate::board::START_FEN.to_string();
        self.played.clear();
        self.redo.clear();
        self.move_history.clear();
        self.interface.clear_history();
        self.interface.clear_highlights();
        self.game_active = true;
    }

    fn play_human_vs_engine(&mut self) {
        while self.game_active {
            self.interface.show_game_screen(&self.board);

            if let Some(result) = self.check_game_end() {
                self.interface.show_game_result(result);
                self.game_active = false;
                break;
            }

            if self.board.side_white {
                if !self.handle_human_move() {
                    break;
                }
            } else {
                self.engine_play_move();
            }
        }
        self.show_end_game_options();
    }

    fn play_human_vs_human(&mut self) {
        while self.game_active {
            self.interface.show_game_screen(&self.board);
            if let Some(result) = self.check_game_end() {
                self.interface.show_game_result(result);
                self.game_active = false;
                break;
            }
            if !self.handle_human_move() {
                break;
            }
        }
        self.show_end_game_options();
    }

    fn play_engine_vs_engine(&mut self) {
        Notification::new(
            "Watching engine battle...".to_string(),
            NotificationKind::Info,
        )
        .show();

        while self.game_active {
            self.interface.show_game_screen(&self.board);
            if let Some(result) = self.check_game_end() {
                self.interface.show_game_result(result);
                self.game_active = false;
                break;
            }
            if !self.engine_play_move() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        self.show_end_game_options();
    }

    /// Have the engine search and play a move. Returns `false` if it had none.
    fn engine_play_move(&mut self) -> bool {
        Notification::new("Engine is thinking...".to_string(), NotificationKind::Info).show();
        let limits = SearchLimits {
            max_depth: self.settings.get_search_depth(),
            movetime_ms: Some(4000),
            verbose: false,
        };
        let start = Instant::now();
        let res = search(&mut self.board, limits);
        self.last_search = Some(res.telemetry);
        let Some(mv) = res.best_move else {
            self.interface.show_error("Engine couldn't find a move!");
            self.game_active = false;
            return false;
        };
        let san = move_to_san(&self.board, mv);
        self.commit(mv);
        Notification::new(
            format!(
                "Engine played {} ({}, depth {}, {} nodes, {}ms)",
                san,
                res.score_string(),
                res.depth,
                res.nodes,
                start.elapsed().as_millis()
            ),
            NotificationKind::Success,
        )
        .show_timed(900);
        true
    }

    /// Free analysis loop. Note: does NOT reset the game, so a position
    /// loaded from PGN (or set up beforehand) survives entering this mode.
    fn analysis_mode(&mut self) {
        loop {
            self.interface.show_game_screen(&self.board);
            let input = self.interface.prompt_input("analysis");
            if input.is_empty() {
                continue;
            }
            let parts: Vec<&str> = input.split_whitespace().collect();

            // This loop redraws the screen every iteration, so shared
            // commands pause for Enter before their output is wiped.
            match self.shared_command(&parts, true) {
                Shared::Redraw | Shared::Stay => continue,
                Shared::NotHandled => {}
            }

            match parts[0] {
                "move" | "m" => {
                    if let Some(s) = parts.get(1) {
                        if let Err(e) = self.try_play(s) {
                            self.interface.show_error(&e);
                            self.pause_enter();
                        }
                    } else {
                        self.interface.show_error("Usage: move e2e4");
                        self.pause_enter();
                    }
                }
                "undo" | "u" => self.undo(),
                "redo" | "r" => self.redo_last(),
                "new" => self.reset_game(),
                "help" => self.interface.show_help(),
                "back" | "exit" | "quit" | "menu" | "resign" => break,
                _ => {
                    if self.try_play(parts[0]).is_err() {
                        self.interface
                            .show_error(&format!("Unknown command or illegal move '{}'", input));
                        self.pause_enter();
                    }
                }
            }
        }
    }

    fn handle_human_move(&mut self) -> bool {
        loop {
            let side = if self.board.side_white { "White" } else { "Black" };
            let input = self.interface.prompt_input(&format!("{} to move", side));
            if input.is_empty() {
                continue;
            }
            let parts: Vec<&str> = input.split_whitespace().collect();

            // This loop keeps prompting without redrawing, so shared command
            // output stays visible; no pause needed.
            match self.shared_command(&parts, false) {
                Shared::Redraw => return true,
                Shared::Stay => continue,
                Shared::NotHandled => {}
            }

            match parts[0] {
                "move" | "m" => {
                    if let Some(s) = parts.get(1) {
                        match self.try_play(s) {
                            Ok(()) => return true,
                            Err(e) => self.interface.show_error(&e),
                        }
                    } else {
                        self.interface.show_error("Usage: move e2e4");
                    }
                }
                "undo" | "u" => {
                    // Against the engine, take back its reply as well so it
                    // is still the human's turn afterwards.
                    self.undo();
                    if self.interface.mode() == GameMode::HumanVsEngine {
                        self.undo();
                    }
                    return true;
                }
                "redo" | "r" => {
                    self.redo_last();
                    if self.interface.mode() == GameMode::HumanVsEngine {
                        self.redo_last();
                    }
                    return true;
                }
                "new" => {
                    if ConfirmDialog::confirm("Start a new game? Current game will be lost.") {
                        self.reset_game();
                        return true;
                    }
                }
                "resign" => {
                    if ConfirmDialog::confirm("Are you sure you want to resign?") {
                        self.interface.show_game_result(GameResult::Resignation);
                        self.game_active = false;
                        return false;
                    }
                }
                "menu" | "quit" | "exit" => {
                    if ConfirmDialog::confirm("Quit current game?") {
                        self.game_active = false;
                        return false;
                    }
                }
                "help" => {
                    self.interface.show_help();
                    return true;
                }
                _ => match self.try_play(parts[0]) {
                    Ok(()) => return true,
                    Err(e) => self
                        .interface
                        .show_error(&format!("Invalid command or move: {}", e)),
                },
            }
        }
    }

    /// Commands available at every in-game prompt (both the play loops and
    /// analysis mode). `pause` should be true when the caller redraws the
    /// screen right after, so printed output waits for Enter first.
    fn shared_command(&mut self, parts: &[&str], pause: bool) -> Shared {
        match parts[0] {
            "hint" | "h" => {
                self.show_hint();
                Shared::Stay
            }
            "analyze" | "a" => {
                let depth = parts
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| self.settings.get_search_depth());
                self.run_analysis(depth);
                Shared::Stay
            }
            "go" => {
                self.run_go(parts, pause);
                Shared::Stay
            }
            "eval" | "e" => {
                self.show_evaluation();
                if pause {
                    self.pause_enter();
                }
                Shared::Stay
            }
            "fen" => {
                self.interface.show_info(&self.board.to_fen());
                if pause {
                    self.pause_enter();
                }
                Shared::Stay
            }
            "hash" => {
                self.interface
                    .show_info(&format!("Zobrist key: 0x{:016x}", self.board.key));
                if pause {
                    self.pause_enter();
                }
                Shared::Stay
            }
            "pgn" => {
                let game = PgnGame::from_moves(&self.start_fen, self.played.clone());
                println!("\n{}", game.to_pgn());
                if pause {
                    self.pause_enter();
                }
                Shared::Stay
            }
            "stats" => {
                self.show_search_stats();
                if pause {
                    self.pause_enter();
                }
                Shared::Stay
            }
            "perft" => {
                let depth = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(4);
                self.run_perft(depth);
                Shared::Stay
            }
            "save" => {
                self.save_game(parts.get(1).copied());
                if pause {
                    self.pause_enter();
                }
                Shared::Stay
            }
            "load" => {
                if self.load_into_state(parts.get(1).copied()) {
                    Shared::Redraw
                } else {
                    if pause {
                        self.pause_enter();
                    }
                    Shared::Stay
                }
            }
            "position" => {
                if self.set_position(parts) {
                    Shared::Redraw
                } else {
                    if pause {
                        self.pause_enter();
                    }
                    Shared::Stay
                }
            }
            "flip" | "f" => {
                self.interface.display.flip_board = !self.interface.display.flip_board;
                Shared::Redraw
            }
            "coords" => {
                let display = &mut self.interface.display;
                display.show_coords = match parts.get(1).copied() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => !display.show_coords,
                };
                Shared::Redraw
            }
            "unicode" => {
                let display = &mut self.interface.display;
                display.use_unicode = match parts.get(1).copied() {
                    Some("on") => true,
                    Some("off") => false,
                    _ => !display.use_unicode,
                };
                Shared::Redraw
            }
            _ => Shared::NotHandled,
        }
    }

    /// `go depth <n>` / `go time <ms>`: run a search with explicit limits and
    /// report it (analysis only; no move is played).
    fn run_go(&mut self, parts: &[&str], pause: bool) {
        let arg = parts.get(2).and_then(|s| s.parse::<u64>().ok());
        let limits = match (parts.get(1).copied(), arg) {
            (Some("depth"), Some(n)) => SearchLimits {
                max_depth: (n as i32).clamp(1, 32),
                movetime_ms: None,
                verbose: false,
            },
            (Some("time"), Some(ms)) => SearchLimits {
                max_depth: 32,
                movetime_ms: Some(ms),
                verbose: false,
            },
            _ => {
                self.interface.show_error("Usage: go depth <n>  or  go time <ms>");
                if pause {
                    self.pause_enter();
                }
                return;
            }
        };
        self.run_search_report(limits);
    }

    /// `position fen <fen>` / `position startpos`: replace the game state.
    fn set_position(&mut self, parts: &[&str]) -> bool {
        let board = match parts.get(1).copied() {
            Some("startpos") => Board::startpos(),
            Some("fen") if parts.len() > 2 => {
                let fen = parts[2..].join(" ");
                match Board::from_fen(&fen) {
                    Ok(b) => b,
                    Err(e) => {
                        self.interface.show_error(&format!("Bad FEN: {}", e));
                        return false;
                    }
                }
            }
            _ => {
                self.interface
                    .show_error("Usage: position fen <FEN>  or  position startpos");
                return false;
            }
        };
        self.start_fen = board.to_fen();
        self.board = board;
        self.played.clear();
        self.redo.clear();
        self.move_history.clear();
        self.interface.clear_history();
        self.interface.clear_highlights();
        true
    }

    fn show_search_stats(&self) {
        match self.last_search {
            Some(t) => StatsDisplay::show_engine_stats(t.nodes, t.elapsed_ms, t.tt_hits, t.tt_probes),
            None => self
                .interface
                .show_info("No search run yet. Try 'analyze', 'hint', or 'go' first."),
        }
    }

    /// Block until the user presses Enter (so output is not wiped by the
    /// next screen redraw).
    fn pause_enter(&self) {
        use crate::ui::colors::*;
        println!("{}Press Enter to continue...{}", DIM, RESET);
        let mut dummy = String::new();
        io::stdin().read_line(&mut dummy).ok();
    }

    /// Parse a UCI or SAN move string, verify legality, and commit it.
    fn try_play(&mut self, move_str: &str) -> Result<(), String> {
        // Accept either UCI (e2e4) or SAN (Nf3) input.
        let mv = self
            .parse_uci_legal(move_str)
            .or_else(|| crate::san::san_to_move(&self.board, move_str))
            .ok_or_else(|| format!("illegal or unrecognized move '{}'", move_str))?;
        self.commit(mv);
        Ok(())
    }

    /// Validate a UCI move string against the legal move list.
    fn parse_uci_legal(&mut self, move_str: &str) -> Option<Move> {
        let (from_str, to_str, promo_char) = InputValidator::validate_move(move_str).ok()?;
        let from = square::from_alg(&from_str)?;
        let to = square::from_alg(&to_str)?;
        let promotion = promo_char.map(|c| Piece::from_char(c.to_ascii_uppercase()));
        let mut legal = Vec::new();
        legal_moves(&mut self.board, &mut legal);
        legal
            .into_iter()
            .find(|m| m.from == from && m.to == to && m.promotion == promotion)
    }

    // -----------------------------------------------------------------------
    // Engine features
    // -----------------------------------------------------------------------

    fn show_hint(&mut self) {
        Notification::new("Calculating best move...".to_string(), NotificationKind::Info).show();
        let limits = SearchLimits {
            max_depth: self.settings.get_search_depth(),
            movetime_ms: Some(2500),
            verbose: false,
        };
        let res = search(&mut self.board, limits);
        self.last_search = Some(res.telemetry);
        match res.best_move {
            Some(mv) => {
                let san = move_to_san(&self.board, mv);
                Notification::new(
                    format!("Suggested: {} ({})", san, res.score_string()),
                    NotificationKind::Success,
                )
                .show_timed(1500);
            }
            None => self.interface.show_error("Could not calculate a hint"),
        }
    }

    fn run_analysis(&mut self, depth: i32) {
        use crate::ui::colors::*;
        println!(
            "{}{}Running analysis to depth {}...{}",
            BOLD, BRIGHT_CYAN, depth, RESET
        );
        self.run_search_report(SearchLimits {
            max_depth: depth,
            movetime_ms: Some(5000),
            verbose: false,
        });
    }

    /// Search with `limits`, render the result panel, record telemetry, and
    /// wait for Enter so the panel can be read.
    fn run_search_report(&mut self, limits: SearchLimits) {
        let res = search(&mut self.board, limits);
        self.last_search = Some(res.telemetry);
        let pv = self.pv_to_san(&res.pv);
        self.interface.display.print_analysis(
            res.depth,
            &res.score_string(),
            res.nodes,
            res.time_ms,
            &pv,
        );
        self.pause_enter();
    }

    /// Render a principal variation (a list of moves) as space-separated SAN.
    fn pv_to_san(&self, pv: &[Move]) -> String {
        let mut b = self.board.clone();
        let mut out = Vec::new();
        for &mv in pv {
            // A PV move could in theory be stale; stop if it is not legal.
            let mut legal = Vec::new();
            legal_moves(&mut b, &mut legal);
            if !legal.contains(&mv) {
                break;
            }
            out.push(move_to_san(&b, mv));
            b.make_move_struct(mv);
        }
        out.join(" ")
    }

    fn show_evaluation(&self) {
        let terms = eval_terms(&self.board);
        StatsDisplay::show_position_eval(terms.material, terms.positional, terms.total);
    }

    fn run_perft(&mut self, depth: u32) {
        use crate::ui::colors::*;
        let start = Instant::now();
        let (entries, total) = crate::perft::divide(&mut self.board, depth);
        for e in &entries {
            println!("  {}{}{}: {}", BRIGHT_GREEN, e.mv, RESET, e.nodes);
        }
        println!(
            "{}perft({}) = {} in {:?}{}",
            BOLD,
            depth,
            total,
            start.elapsed(),
            RESET
        );
        self.pause_enter();
    }

    fn check_game_end(&mut self) -> Option<GameResult> {
        match game_status(&mut self.board) {
            GameStatus::Ongoing => None,
            GameStatus::Checkmate { winner } => Some(if winner == Color::White {
                GameResult::WhiteWins
            } else {
                GameResult::BlackWins
            }),
            GameStatus::Stalemate => Some(GameResult::Stalemate),
            GameStatus::DrawFiftyMove
            | GameStatus::DrawRepetition
            | GameStatus::DrawInsufficientMaterial => Some(GameResult::Draw),
        }
    }

    // -----------------------------------------------------------------------
    // Persistence (PGN)
    // -----------------------------------------------------------------------

    fn show_end_game_options(&mut self) {
        let options = vec!["Save Game (PGN)", "Show PGN", "Main Menu"];
        let choice = ConfirmDialog::choose("What would you like to do?", &options);
        match choice {
            0 => self.save_game(None),
            1 => {
                let game = PgnGame::from_moves(&self.start_fen, self.played.clone());
                println!("\n{}", game.to_pgn());
                self.pause_enter();
            }
            _ => {}
        }
    }

    fn save_game(&self, filename: Option<&str>) {
        let filename = match filename {
            Some(f) => f.to_string(),
            None => self.interface.prompt_input("Save as (filename)"),
        };
        if filename.is_empty() {
            self.interface.show_warning("Save cancelled");
            return;
        }
        let path = if filename.ends_with(".pgn") {
            filename
        } else {
            format!("{}.pgn", filename)
        };
        let game = PgnGame::from_moves(&self.start_fen, self.played.clone());
        match std::fs::write(&path, game.to_pgn()) {
            Ok(()) => {
                // Report the full path so the user knows where to find (and
                // later load) the game.
                let shown = std::fs::canonicalize(&path)
                    .map(|p| p.display().to_string())
                    .unwrap_or(path);
                self.interface.show_success(&format!("Saved to {}", shown));
            }
            Err(e) => self.interface.show_error(&format!("Could not save: {}", e)),
        }
    }

    /// Main-menu entry: load a PGN and open it in analysis mode.
    fn load_game(&mut self) {
        if self.load_into_state(None) {
            self.interface.set_game_mode(GameMode::Analysis);
            self.game_active = true;
            self.analysis_mode();
        }
    }

    /// `.pgn` files in the current directory (where `save` writes), sorted.
    fn list_saved_games() -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(".")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                name.ends_with(".pgn").then_some(name)
            })
            .collect();
        names.sort();
        names
    }

    fn show_saved_games(&self) {
        let saves = Self::list_saved_games();
        if saves.is_empty() {
            let here = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| ".".to_string());
            self.interface
                .show_info(&format!("No saved games (.pgn) found in {}", here));
        } else {
            self.interface
                .show_info(&format!("Saved games here: {}", saves.join(", ")));
        }
    }

    /// Load a PGN file and replace the current game state with it, prompting
    /// for the filename when none is given. Returns true on success. Used
    /// both from the main menu and by the in-game `load` command (which then
    /// continues in the current mode).
    fn load_into_state(&mut self, filename: Option<&str>) -> bool {
        let filename = match filename {
            Some(f) => f.to_string(),
            None => {
                self.show_saved_games();
                self.interface.prompt_input("Load PGN (filename)")
            }
        };
        if filename.is_empty() {
            self.interface.show_warning("Load cancelled");
            return false;
        }
        let path = if filename.ends_with(".pgn") {
            filename
        } else {
            format!("{}.pgn", filename)
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                self.interface
                    .show_error(&format!("Could not read {}: {}", path, e));
                self.show_saved_games();
                return false;
            }
        };
        match PgnGame::parse(&text) {
            Ok(game) => {
                self.start_fen = game.start_fen.clone();
                self.board = Board::from_fen(&self.start_fen).unwrap_or_else(|_| Board::startpos());
                self.played.clear();
                self.redo.clear();
                for mv in game.moves {
                    self.board.make_move_struct(mv);
                    self.played.push(mv);
                }
                self.rebuild_history();
                match self.played.last() {
                    Some(last) => self.interface.highlight_move(last.from, last.to),
                    None => self.interface.clear_highlights(),
                }
                self.interface
                    .show_success(&format!("Loaded {} ({} moves)", path, self.played.len()));
                true
            }
            Err(e) => {
                self.interface.show_error(&format!("Bad PGN: {}", e));
                false
            }
        }
    }

    // -----------------------------------------------------------------------
    // Misc menu screens
    // -----------------------------------------------------------------------

    fn configure_settings(&mut self) {
        self.settings = GameSettings::configure_interactive();
        Notification::new("Settings updated!".to_string(), NotificationKind::Success).show();
    }

    fn show_tutorial(&self) {
        self.interface.show_help();
    }

    fn show_statistics(&self) {
        StatsDisplay::show_engine_stats(0, 0, 0, 0);
    }

    fn show_about(&self) {
        use crate::ui::colors::*;
        use crate::ui::{text, PANEL_WIDTH};
        let inner = PANEL_WIDTH - 2;
        println!();
        println!(
            "{}{}╔{}╗{}",
            BOLD,
            BRIGHT_CYAN,
            "═".repeat(inner),
            RESET
        );
        println!(
            "{}{}║{}║{}",
            BOLD,
            BRIGHT_CYAN,
            text::center("RUST CHESS ENGINE", inner),
            RESET
        );
        println!(
            "{}{}╚{}╝{}",
            BOLD,
            BRIGHT_CYAN,
            "═".repeat(inner),
            RESET
        );
        println!();
        println!("  {}Features:{}", BOLD, RESET);
        println!("    • 0x88 board, perft-verified legal move generation");
        println!("    • Iterative-deepening PVS + transposition table + Zobrist");
        println!("    • Tapered piece-square-table evaluation");
        println!("    • SAN/PGN, UCI protocol, opening book");
        println!("    • Interactive terminal UI with multiple game modes");
        println!();
        self.pause_enter();
    }

    /// 0x88 square to algebraic, retained for any external callers.
    #[allow(dead_code)]
    fn sq_to_alg(s: Sq) -> String {
        square::to_alg(s)
    }
}

impl Default for GameController {
    fn default() -> Self {
        Self::new()
    }
}
