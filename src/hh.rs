//! GGPoker hand-history import.
//!
//! PokerCraft exports arrive as zip files (one or more `.txt` entries each)
//! in the `history/` directory. Two kinds of text live inside them:
//!
//! * hand files — blocks starting with `Poker Hand #SG...:` carrying the
//!   tournament id, blinds, seats, action lines, and the per-hand result;
//! * tournament-summary files — starting with `Tournament #...`, carrying
//!   the buy-in, prize pool, and the hero's finish position.
//!
//! Everything keys off PokerCraft's natural identifiers (the `SG...` hand ids
//! and the tournament numbers), so importing the same zip twice inserts
//! nothing new — the importer reports how many hands were new and how many
//! were already imported. Timestamps are stored as PokerCraft prints them
//! (normalized to dashes); money is stored in cents.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use sqlx::PgPool;
use zip::ZipArchive;

use crate::error::Result;

/// The directory scanned for PokerCraft export zips when none is configured.
pub fn default_history_dir() -> PathBuf {
    PathBuf::from("history")
}

// ---------------------------------------------------------------- parsing

/// One parsed GGPoker hand, as read from a hand-history text block.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedHand {
    pub hand_id: String,
    pub tournament_id: String,
    pub tournament_name: String,
    pub game_type: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS` (slash dates are normalized to dashes).
    pub played_at: String,
    pub sb: i32,
    pub bb: i32,
    /// Hero's position: `BTN`, `SB`, or `BB`.
    pub position: String,
    pub table_size: i32,
    /// Hero's chips at the start of the hand.
    pub hero_stack: Option<i32>,
    pub hero_cards: Option<String>,
    pub all_in: bool,
    /// Whether the hero reached showdown (i.e. did not fold).
    pub showdown: bool,
    pub hero_won: bool,
    /// Chips the hero committed to the pot (uncalled returns already back).
    pub invested: i32,
    /// Chips the hero collected back from the pot.
    pub collected: i32,
    /// `collected - invested`: the hero's chip result of the hand.
    pub net: i32,
    pub board: Option<String>,
    /// The raw hand block as PokerCraft wrote it.
    pub raw: String,
}

/// One parsed tournament-summary block.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedTournament {
    pub id: String,
    pub name: String,
    pub game_type: Option<String>,
    /// `YYYY-MM-DD HH:MM:SS`, from `Tournament started ...`.
    pub started: Option<String>,
    pub buy_in_cents: Option<i32>,
    pub prize_cents: Option<i32>,
    /// The hero's finish position (1 = won).
    pub place: Option<i32>,
    pub entrants: Option<i32>,
}

/// One `.txt` entry from one scanned zip, parsed into hands and/or a
/// tournament summary.
#[derive(Clone, Debug, PartialEq)]
pub struct ScannedFile {
    pub zip_name: String,
    pub entry_name: String,
    pub hands: Vec<ParsedHand>,
    pub tournament: Option<ParsedTournament>,
}

/// The outcome of scanning the history directory: every recognized file plus
/// one message per unreadable or unrecognizable zip/entry, in scan order.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanRun {
    pub files: Vec<ScannedFile>,
    pub failures: Vec<String>,
    pub zips: usize,
}

/// Recursively finds every `.zip` under `base` and parses every `.txt` entry.
/// A missing directory is an empty scan, not an error; per-file problems are
/// collected into [`ScanRun::failures`] so one bad zip never blocks the rest.
pub fn scan_directory(base: &Path) -> Result<ScanRun> {
    let mut zips_paths = Vec::new();
    if base.is_dir() {
        collect_zips(base, &mut zips_paths);
    }
    zips_paths.sort();
    let zips = zips_paths.len();

    let mut files = Vec::new();
    let mut failures = Vec::new();

    for path in zips_paths {
        let zip_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("?")
            .to_string();
        let handle = match std::fs::File::open(&path) {
            Ok(handle) => handle,
            Err(error) => {
                failures.push(format!("{zip_name}: {error}"));
                continue;
            }
        };
        let mut archive = match ZipArchive::new(handle) {
            Ok(archive) => archive,
            Err(error) => {
                failures.push(format!("{zip_name}: {error}"));
                continue;
            }
        };
        for index in 0..archive.len() {
            let entry = match archive.by_index(index) {
                Ok(entry) => entry,
                Err(error) => {
                    failures.push(format!("{zip_name}: {error}"));
                    continue;
                }
            };
            let entry_name = entry.name().to_string();
            if entry.is_dir() || !entry_name.to_ascii_lowercase().ends_with(".txt") {
                continue;
            }
            let mut content = String::new();
            let mut entry = entry;
            if let Err(error) = entry.read_to_string(&mut content) {
                failures.push(format!("{zip_name}/{entry_name}: {error}"));
                continue;
            }
            let parsed = parse_file_content(&content);
            if parsed.hands.is_empty() && parsed.tournament.is_none() {
                failures.push(format!(
                    "{zip_name}/{entry_name}: no recognizable PokerCraft content"
                ));
                continue;
            }
            files.push(ScannedFile {
                zip_name: zip_name.clone(),
                entry_name,
                hands: parsed.hands,
                tournament: parsed.tournament,
            });
        }
    }

    Ok(ScanRun {
        files,
        failures,
        zips,
    })
}

fn collect_zips(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            collect_zips(&path, out);
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
        {
            out.push(path);
        }
    }
}

/// One parsed file: hands and/or a tournament summary.
#[derive(Clone, Debug, PartialEq)]
pub struct FileParse {
    pub hands: Vec<ParsedHand>,
    pub tournament: Option<ParsedTournament>,
}

/// Splits one export text into parsed hands and/or a tournament summary,
/// depending on which kind of PokerCraft file it is.
pub fn parse_file_content(text: &str) -> FileParse {
    if text.trim_start().starts_with("Tournament #") {
        FileParse {
            hands: Vec::new(),
            tournament: parse_tournament_summary(text),
        }
    } else {
        FileParse {
            hands: parse_hands(text),
            tournament: None,
        }
    }
}

/// Parses every `Poker Hand #...` block in a hand-history text. Blocks that
/// cannot be parsed to a well-formed hand are skipped.
pub fn parse_hands(text: &str) -> Vec<ParsedHand> {
    let mut blocks: Vec<&str> = text.trim().split("\nPoker Hand #").collect();
    if let Some(first) = blocks.first_mut() {
        *first = first.strip_prefix("Poker Hand #").unwrap_or(first);
    }
    blocks
        .iter()
        .filter_map(|block| parse_hand(block))
        .collect()
}

fn parse_hand(block: &str) -> Option<ParsedHand> {
    let block = block.trim();
    let mut lines = block.lines();
    let header = lines.next()?;
    let header = parse_header(header)?;

    let mut seats: Vec<(i32, String, Option<i32>)> = Vec::new();
    let mut hero_invested = 0i32;
    let mut hero_collected = 0i32;
    let mut hero_all_in = false;
    let mut hero_folded = false;
    let mut hero_cards = None;
    let mut board = None;
    let mut at_summary = false;

    for line in lines {
        let line = line.trim();
        if line.starts_with("*** SUMMARY ***") {
            at_summary = true;
        }
        if at_summary {
            if let Some(rest) = line.strip_prefix("Board [") {
                board = rest.strip_suffix(']').map(str::to_string);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("Seat ") {
            // "2: Hero (525 in chips)"
            let (seat_no, rest) = rest.trim_start().split_once(':')?;
            let seat_no: i32 = seat_no.trim().parse().ok()?;
            let rest = rest.trim();
            let name_end = rest.find(" (").unwrap_or(rest.len());
            let name = rest[..name_end].to_string();
            let stack = rest[name_end..].split_whitespace().next().and_then(|word| {
                word.trim_matches(|c: char| !c.is_ascii_digit())
                    .parse()
                    .ok()
            });
            seats.push((seat_no, name, stack));
            continue;
        }
        if let Some(action) = line.strip_prefix("Hero: ") {
            let words: Vec<&str> = action.split_whitespace().collect();
            if words.first().copied() == Some("folds") {
                hero_folded = true;
            } else {
                hero_invested += hero_commitment(action, hero_invested);
                if action.contains("is all-in") {
                    hero_all_in = true;
                }
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("Dealt to Hero [") {
            hero_cards = rest.strip_suffix(']').map(str::to_string);
            continue;
        }
        if line.starts_with("Uncalled bet") && line.contains("returned to Hero") {
            hero_invested -= first_number_plain(line).unwrap_or(0);
            continue;
        }
        if line.starts_with("Hero collected") {
            hero_collected += number_after(line, "collected").unwrap_or(0);
        }
    }

    let hero = seats.iter().find(|(_, name, _)| name == "Hero")?;
    let hero_stack = hero.2;

    let position = position_of(block);

    let net = hero_collected - hero_invested;
    Some(ParsedHand {
        hand_id: header.hand_id,
        tournament_id: header.tournament_id,
        tournament_name: header.name,
        game_type: header.game,
        played_at: header.played_at,
        sb: header.sb,
        bb: header.bb,
        position,
        table_size: seats.len() as i32,
        hero_stack,
        hero_cards,
        all_in: hero_all_in,
        showdown: !hero_folded && block.contains(": shows ["),
        hero_won: net > 0,
        invested: hero_invested,
        collected: hero_collected,
        net,
        board,
        raw: block.to_string(),
    })
}

struct Header {
    hand_id: String,
    tournament_id: String,
    name: String,
    game: Option<String>,
    sb: i32,
    bb: i32,
    played_at: String,
}

fn parse_header(line: &str) -> Option<Header> {
    let line = line.strip_prefix("Poker Hand #").unwrap_or(line);
    let (hand_id, tail) = line.split_once(": ")?;
    let (tournament_id, rest) = tail.trim().strip_prefix("Tournament #")?.split_once(',')?;
    let rest = rest.trim();
    let (name_game, tail) = rest.split_once(" - Level")?;
    let (level, date) = tail.split_once(") - ")?;
    let open = level.find('(')?;
    let blinds = &level[open + 1..];
    let (sb, bb) = blinds.split_once('/')?;
    Some(Header {
        hand_id: hand_id.trim().to_string(),
        tournament_id: tournament_id.trim().to_string(),
        name: split_name_game(name_game).0,
        game: split_name_game(name_game).1,
        sb: sb.trim().parse().ok()?,
        bb: bb.trim().parse().ok()?,
        played_at: normalize_date(date.trim()),
    })
}

/// Splits `Spin&Gold #7 Hold'em No Limit` into name and game type; without a
/// recognized game suffix the whole string is the name.
fn split_name_game(part: &str) -> (String, Option<String>) {
    let tokens: Vec<&str> = part.split_whitespace().collect();
    let n = tokens.len();
    if let Some(hash) = tokens.iter().rposition(|token| token.starts_with('#')) {
        let game = (hash + 1 < n).then(|| tokens[hash + 1..].join(" "));
        return (tokens[..=hash].join(" "), game);
    }
    if n >= 2 && matches!(tokens[n - 2], "No" | "Pot") && tokens[n - 1] == "Limit" {
        (tokens[..n - 2].join(" "), Some(tokens[n - 2..].join(" ")))
    } else {
        (tokens.join(" "), None)
    }
}

/// `2026/08/21 15:07:44` → `2026-08-21 15:07:44` so every stored timestamp
/// sorts lexicographically.
fn normalize_date(date: &str) -> String {
    date.trim().replace('/', "-")
}

/// The chips a hero action line commits: a raise's `to` amount is the hero's
/// total commitment at that point (its increment is `to - invested`), while
/// posts, calls, and bets commit their stated amount.
fn hero_commitment(action: &str, invested: i32) -> i32 {
    let words: Vec<&str> = action.split_whitespace().collect();
    match words.first().copied() {
        Some("posts") => words
            .iter()
            .rev()
            .find_map(|word| parse_i32(word))
            .unwrap_or(0),
        Some("calls") | Some("bets") => words.iter().find_map(|word| parse_i32(word)).unwrap_or(0),
        Some("raises") => {
            let to = words
                .iter()
                .position(|word| *word == "to")
                .and_then(|to| words[to + 1..].iter().find_map(|word| parse_i32(word)));
            to.map(|to| (to - invested).max(0)).unwrap_or(0)
        }
        _ => 0,
    }
}

/// First integer in a string, tolerating decorations like `(15)`.
fn first_number_plain(text: &str) -> Option<i32> {
    text.split_whitespace().find_map(parse_i32)
}

/// The first integer after the given keyword.
fn number_after(text: &str, keyword: &str) -> Option<i32> {
    let at = text.find(keyword)?;
    text[at + keyword.len()..]
        .split_whitespace()
        .find_map(parse_i32)
}

fn parse_i32(word: &str) -> Option<i32> {
    word.trim_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

/// The hero's position: from his blind post (heads-up the small blind is the
/// button, which is why the post wins), button otherwise.
fn position_of(block: &str) -> String {
    for line in block.lines().map(str::trim) {
        if line.starts_with("Hero: posts small blind") {
            return "SB".to_string();
        }
        if line.starts_with("Hero: posts big blind") {
            return "BB".to_string();
        }
    }
    "BTN".to_string()
}

/// Parses a PokerCraft tournament-summary block (buy-in, prize, finish).
pub fn parse_tournament_summary(text: &str) -> Option<ParsedTournament> {
    let mut lines = text.lines().map(str::trim);
    let header = lines.next()?;
    let rest = header.strip_prefix("Tournament #")?;
    let (id, rest) = rest.split_once(',')?;
    let rest = rest.trim();
    let mut parts = rest.split(", ");
    let name = parts.next()?.trim().to_string();
    let game = parts
        .next()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string);

    let mut started = None;
    let mut buy_in_cents = None;
    let mut prize_cents = None;
    let mut place = None;
    let mut entrants = None;

    for line in lines {
        if let Some(rest) = line.strip_prefix("Buy-in: $") {
            buy_in_cents = parse_cents(rest);
        } else if let Some(rest) = line.strip_prefix("Tournament started ") {
            started = Some(normalize_date(rest));
        } else if line.contains(": Hero, $") {
            prize_cents = line.rsplit(", $").next().and_then(parse_cents);
        } else if line.starts_with("You finished in ") {
            place = first_number_plain(line);
        } else if let Some(rest) = line.strip_suffix(" Players") {
            entrants = parse_i32_end(rest);
        }
    }

    Some(ParsedTournament {
        id: id.trim().to_string(),
        name,
        game_type: game,
        started,
        buy_in_cents,
        prize_cents,
        place,
        entrants,
    })
}

fn parse_cents(text: &str) -> Option<i32> {
    let value: f64 = text.trim().parse().ok()?;
    Some((value * 100.0).round() as i32)
}

// -------------------------------------------------------------- episodes

/// One seat declaration from a hand block (`Seat 2: Hero (525 in chips)`).
#[derive(Clone, Debug, PartialEq)]
pub struct EpisodeSeat {
    pub no: u8,
    pub name: String,
    pub stack: Option<i32>,
}

/// The verb of one action line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EpisodeVerb {
    /// A blind or ante post (`posts small blind 20`).
    Post,
    Fold,
    Check,
    Call,
    Bet,
    Raise,
}

/// One action line: `{name}: posts small blind 20`, `14c11a2a: raises 175 to
/// 215 and is all-in`, and so on.
#[derive(Clone, Debug, PartialEq)]
pub struct EpisodeAction {
    pub seat_no: u8,
    pub verb: EpisodeVerb,
    /// Commit for posts/calls/bets; the raise increment for raises.
    pub amount: Option<i32>,
    /// The raise-to amount for raises.
    pub to: Option<i32>,
    pub all_in: bool,
}

/// One hand block reduced to its full action timeline. This is the input for
/// the opponent-skill replayer: it reconstructs every decision point in the
/// hand so each opponent action can be graded against the solver.
#[derive(Clone, Debug, PartialEq)]
pub struct Episode {
    pub seats: Vec<EpisodeSeat>,
    /// Street markers with the board-so-far: `(1 = flop, 2 = turn, 3 = river,
    /// cumulative cards)`.
    pub boards: Vec<(u8, Vec<String>)>,
    pub hero_cards: Option<[String; 2]>,
    /// Every action including the hero's — the hero's decisions are needed to
    /// replay the hand but are never graded themselves.
    pub actions: Vec<EpisodeAction>,
    /// The declared button seat from `Table '...' Seat #N is the button`,
    /// when the line is present.
    pub button: Option<u8>,
    /// `Board [...]` from the summary, as a fallback when street markers
    /// carry no board.
    pub summary_board: Option<Vec<String>>,
}

/// Splits a hand block into its full action timeline.
pub fn parse_episode(block: &str) -> Option<Episode> {
    let mut seats: Vec<EpisodeSeat> = Vec::new();
    let mut actions: Vec<EpisodeAction> = Vec::new();
    let mut boards: Vec<(u8, Vec<String>)> = Vec::new();
    let mut hero_cards = None;
    let mut button = None;
    let mut summary_board = None;
    let mut at_summary = false;

    for (index, line) in block.lines().map(str::trim).enumerate() {
        // The stored raw blocks drop the `Poker Hand #` prefix: the header is
        // the first line, with or without it.
        if line.starts_with("Poker Hand #") {
            continue;
        }
        if index == 0 && line.contains(": Tournament #") {
            continue;
        }
        if line.starts_with("*** SUMMARY ***") {
            at_summary = true;
        }
        if at_summary {
            if line.starts_with("Board [") {
                summary_board = Some(cards_in_brackets(line));
            }
            continue;
        }
        if line.starts_with("Dealt to Hero [") {
            let cards = cards_in_brackets(line);
            hero_cards = two_cards(&cards);
            continue;
        }
        if let Some(rest) = line.strip_prefix("Seat ") {
            if let Some(seat) = parse_episode_seat(rest) {
                seats.push(seat);
            }
            continue;
        }
        if let Some(seat_no) = parse_button_line(line) {
            button = Some(seat_no);
            continue;
        }
        if let Some((street, cards)) = parse_street_marker(line) {
            let card_list = cards_in_brackets(&cards);
            if !card_list.is_empty() {
                boards.push((street, card_list));
            }
            continue;
        }
        if let Some((name, text)) = line.split_once(": ") {
            let seat_no = seats.iter().find(|seat| seat.name == name)?.no;
            if let Some(action) = parse_episode_action(text) {
                actions.push(EpisodeAction { seat_no, ..action });
            }
        }
    }

    if seats.is_empty() {
        return None;
    }
    Some(Episode {
        seats,
        boards,
        hero_cards,
        actions,
        button,
        summary_board,
    })
}

/// Parses `Seat 2: Hero (525 in chips)` (the parenthesized stack is
/// optional).
fn parse_episode_seat(rest: &str) -> Option<EpisodeSeat> {
    let (seat_no, rest) = rest.trim_start().split_once(':')?;
    let no: u8 = seat_no.trim().parse().ok()?;
    let rest = rest.trim();
    let name_end = rest.find('(').unwrap_or(rest.len());
    let name = rest[..name_end].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let stack = rest[name_end..].split_whitespace().find_map(|word| {
        word.trim_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .ok()
    });
    Some(EpisodeSeat { no, name, stack })
}

/// Parses `Table '39856' 3-max Seat #2 is the button` into its seat number.
fn parse_button_line(line: &str) -> Option<u8> {
    let (_, rest) = line.split_once("Seat #")?;
    rest.split_whitespace().next()?.parse().ok()
}

/// Parses `*** FLOP *** [Jd 3c 8c]` (and turn/river markers) into the street
/// index plus the raw bracket tail.
fn parse_street_marker(line: &str) -> Option<(u8, String)> {
    let street = if line.starts_with("*** FLOP ***") {
        1
    } else if line.starts_with("*** TURN ***") {
        2
    } else if line.starts_with("*** RIVER ***") {
        3
    } else {
        return None;
    };
    let open = line.find('[')?;
    Some((street, line[open..].to_string()))
}

/// All card codes inside `[...]` groups, in reading order.
fn cards_in_brackets(text: &str) -> Vec<String> {
    let mut cards = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        for code in after[..close].split_whitespace() {
            if !code.is_empty() {
                cards.push(code.to_string());
            }
        }
        rest = &after[close + 1..];
    }
    cards
}

/// The first two cards of a bracket-derived code list, i.e. `[As, Kh]`.
fn two_cards(cards: &[String]) -> Option<[String; 2]> {
    Some([cards.first()?.clone(), cards.get(1)?.clone()])
}

/// Parses the text after `{name}: ` into an action (`posts small blind 20`,
/// `folds`, `checks`, `calls 20`, `bets 40`, `raises 40 to 80`), with the
/// trailing `and is all-in` extracted.
fn parse_episode_action(text: &str) -> Option<EpisodeAction> {
    let (text, all_in) = match text.strip_suffix(" and is all-in") {
        Some(stripped) => (stripped, true),
        None => (text, false),
    };
    let words: Vec<&str> = text.split_whitespace().collect();
    let first = *words.first()?;
    let integer_at = |index: usize| words.get(index).copied().and_then(parse_i32);
    let last_integer = || words.iter().rev().copied().find_map(parse_i32);

    let (verb, amount, to) = match first {
        "posts" => (EpisodeVerb::Post, last_integer(), None),
        "folds" => (EpisodeVerb::Fold, None, None),
        "checks" => (EpisodeVerb::Check, None, None),
        "calls" => (EpisodeVerb::Call, last_integer(), None),
        "bets" => (EpisodeVerb::Bet, last_integer(), None),
        "raises" => {
            let raise_by = words.iter().copied().find_map(parse_i32);
            let to = words
                .iter()
                .position(|word| *word == "to")
                .and_then(|at| integer_at(at + 1));
            (EpisodeVerb::Raise, raise_by, to)
        }
        _ => return None,
    };
    Some(EpisodeAction {
        seat_no: 0,
        verb,
        amount,
        to,
        all_in,
    })
}

/// Parses a leading integer (`"3"` → 3).
fn parse_i32_end(text: &str) -> Option<i32> {
    text.trim().parse().ok()
}

// ------------------------------------------------------------- persistence

/// What one import run stored: parse counts, database counts, and the
/// statistics over the newly imported hands only.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportOutcome {
    pub zips: usize,
    pub files: usize,
    pub hands_parsed: usize,
    pub tournaments_parsed: usize,
    /// Hands stored for the first time.
    pub hands_new: usize,
    /// Parsed hands that were already in the database.
    pub hands_skipped: usize,
    /// Tournaments seen for the first time.
    pub tournaments_new: usize,
    pub failures: Vec<String>,
    /// Aggregates restricted to the newly imported hands.
    pub new_stats: NewHandStats,
}

/// Statistics over a set of hands (the newly imported ones on the results
/// page).
#[derive(Clone, Debug, PartialEq)]
pub struct NewHandStats {
    pub hands: i64,
    pub won: i64,
    pub lost: i64,
    /// Percentage of hands won, 0..100.
    pub win_ratio: f64,
    pub all_ins: i64,
    pub showdowns: i64,
    pub invested: i64,
    pub collected: i64,
    /// Total chip result (`collected - invested`).
    pub net_chips: i64,
    /// Distinct tournaments the hands belong to.
    pub tournaments: i64,
}

/// Aggregates a set of parsed hands, restricted to new hand ids when given.
pub fn aggregate_hands(
    hands: &[ParsedHand],
    only_new: Option<&std::collections::HashSet<String>>,
) -> NewHandStats {
    let mut seen: HashMap<&str, &ParsedHand> = HashMap::new();
    for hand in hands {
        if only_new.is_some_and(|new| !new.contains(&hand.hand_id)) {
            continue;
        }
        seen.insert(hand.hand_id.as_str(), hand);
    }
    let mut stats = NewHandStats {
        hands: 0,
        won: 0,
        lost: 0,
        win_ratio: 0.0,
        all_ins: 0,
        showdowns: 0,
        invested: 0,
        collected: 0,
        net_chips: 0,
        tournaments: 0,
    };
    let mut tournament_ids: Vec<&str> = Vec::new();
    for hand in seen.values() {
        stats.hands += 1;
        if hand.hero_won {
            stats.won += 1;
        } else {
            stats.lost += 1;
        }
        if hand.all_in {
            stats.all_ins += 1;
        }
        if hand.showdown {
            stats.showdowns += 1;
        }
        stats.invested += i64::from(hand.invested);
        stats.collected += i64::from(hand.collected);
        stats.net_chips += i64::from(hand.net);
        if !tournament_ids.contains(&hand.tournament_id.as_str()) {
            tournament_ids.push(hand.tournament_id.as_str());
        }
    }
    stats.tournaments = tournament_ids.len() as i64;
    if stats.hands > 0 {
        stats.win_ratio = stats.won as f64 * 100.0 / stats.hands as f64;
    }
    stats
}

/// One raw tournament row as stored in `gg_tournaments`.
#[derive(Clone, Debug, PartialEq)]
pub struct TournamentSummary {
    pub id: String,
    pub name: String,
    pub game_type: Option<String>,
    pub started: String,
    pub finished: Option<String>,
    pub buy_in_cents: Option<i32>,
    pub prize_cents: Option<i32>,
    pub place: Option<i32>,
    pub entrants: Option<i32>,
}

/// One row of the hand-history listing: the tournament plus its hand-level
/// aggregates.
#[derive(Clone, Debug, PartialEq)]
pub struct TournamentListing {
    pub tournament: TournamentSummary,
    pub hands: i64,
    pub hands_won: i64,
    pub all_ins: i64,
    pub showdowns: i64,
    pub net_chips: i64,
}

/// One stored hand, as the tournament detail page renders it.
#[derive(Clone, Debug, PartialEq)]
pub struct HandRow {
    pub hand_id: String,
    pub played_at: String,
    pub sb: i32,
    pub bb: i32,
    pub position: String,
    pub table_size: i32,
    pub hero_stack: Option<i32>,
    pub hero_cards: Option<String>,
    pub all_in: bool,
    pub showdown: bool,
    pub hero_won: bool,
    pub invested: i32,
    pub collected: i32,
    pub net: i32,
    pub board: Option<String>,
}

/// One tournament's full detail: the stored summary, its aggregates, and its
/// hands newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct TournamentDetail {
    pub listing: TournamentListing,
    pub hands: Vec<HandRow>,
}

/// Caret aggregated across every imported tournament.
#[derive(Clone, Debug, PartialEq)]
pub struct OverallStats {
    pub tournaments: i64,
    pub tournaments_won: i64,
    pub hands: i64,
    pub hands_won: i64,
    pub showdowns: i64,
    pub all_ins: i64,
    pub buy_in_cents: i64,
    pub prize_cents: i64,
    pub invested: i64,
    pub collected: i64,
    pub net_chips: i64,
}

/// Imports one scan into the database and reports what was new. Idempotent:
/// hands already present are skipped (`ON CONFLICT DO NOTHING`), tournament
/// rows are merged (later summary files fill in buy-in/prize/place).
pub async fn import_scan(pool: &PgPool, run: &ScanRun) -> Result<ImportOutcome> {
    let mut tournaments: BTreeMap<String, TournamentBuild> = BTreeMap::new();
    let mut hands: Vec<ParsedHand> = Vec::new();

    for file in &run.files {
        for hand in &file.hands {
            tournaments
                .entry(hand.tournament_id.clone())
                .and_modify(|build| build.merge_hand(hand))
                .or_insert_with(|| TournamentBuild::from_hand(hand));
            hands.push(hand.clone());
        }
        if let Some(summary) = &file.tournament {
            tournaments
                .entry(summary.id.clone())
                .and_modify(|build| build.merge_summary(summary))
                .or_insert_with(|| TournamentBuild::from_summary(summary));
        }
    }

    let hands_parsed = hands.len();
    let tournaments_parsed = tournaments.len();

    let mut transaction = pool.begin().await?;

    let mut tournaments_new = 0usize;
    for build in tournaments.values() {
        let inserted: bool = sqlx::query_scalar(
            "INSERT INTO gg_tournaments
                 (id, name, game_type, started_at, finished_at, buy_in_cents, prize_cents, place, entrants)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (id) DO UPDATE SET
                 name = EXCLUDED.name,
                 game_type = COALESCE(EXCLUDED.game_type, gg_tournaments.game_type),
                 started_at = CASE
                     WHEN gg_tournaments.started_at = '1970-01-01 00:00:00' THEN EXCLUDED.started_at
                     ELSE LEAST(gg_tournaments.started_at, EXCLUDED.started_at)
                 END,
                 finished_at = GREATEST(
                     COALESCE(gg_tournaments.finished_at, EXCLUDED.finished_at),
                     COALESCE(EXCLUDED.finished_at, gg_tournaments.finished_at)
                 ),
                 buy_in_cents = COALESCE(EXCLUDED.buy_in_cents, gg_tournaments.buy_in_cents),
                 prize_cents = COALESCE(EXCLUDED.prize_cents, gg_tournaments.prize_cents),
                 place = COALESCE(EXCLUDED.place, gg_tournaments.place),
                 entrants = COALESCE(EXCLUDED.entrants, gg_tournaments.entrants),
                 updated_at = now()
             RETURNING (xmax = 0)",
        )
        .bind(&build.id)
        .bind(&build.name)
        .bind(&build.game)
        .bind(&build.started)
        .bind(&build.finished)
        .bind(build.buy_in_cents)
        .bind(build.prize_cents)
        .bind(build.place)
        .bind(build.entrants)
        .fetch_one(&mut *transaction)
        .await?;
        if inserted {
            tournaments_new += 1;
        }
    }

    let new_hand_ids: std::collections::HashSet<String> = if hands.is_empty() {
        std::collections::HashSet::new()
    } else {
        let ids: Vec<String> = hands.iter().map(|hand| hand.hand_id.clone()).collect();
        let tournament_ids: Vec<String> = hands
            .iter()
            .map(|hand| hand.tournament_id.clone())
            .collect();
        let played_ats: Vec<String> = hands.iter().map(|hand| hand.played_at.clone()).collect();
        let sbs: Vec<i32> = hands.iter().map(|hand| hand.sb).collect();
        let bbs: Vec<i32> = hands.iter().map(|hand| hand.bb).collect();
        let positions: Vec<String> = hands.iter().map(|hand| hand.position.clone()).collect();
        let table_sizes: Vec<i32> = hands.iter().map(|hand| hand.table_size).collect();
        let hero_stacks: Vec<Option<i32>> = hands.iter().map(|hand| hand.hero_stack).collect();
        let hero_cards: Vec<Option<String>> =
            hands.iter().map(|hand| hand.hero_cards.clone()).collect();
        let all_ins: Vec<bool> = hands.iter().map(|hand| hand.all_in).collect();
        let showdowns: Vec<bool> = hands.iter().map(|hand| hand.showdown).collect();
        let hero_wons: Vec<bool> = hands.iter().map(|hand| hand.hero_won).collect();
        let investeds: Vec<i32> = hands.iter().map(|hand| hand.invested).collect();
        let collecteds: Vec<i32> = hands.iter().map(|hand| hand.collected).collect();
        let nets: Vec<i32> = hands.iter().map(|hand| hand.net).collect();
        let boards: Vec<Option<String>> = hands.iter().map(|hand| hand.board.clone()).collect();
        let raws: Vec<String> = hands.iter().map(|hand| hand.raw.clone()).collect();

        sqlx::query_scalar(
            "INSERT INTO gg_hands
                 (hand_id, tournament_id, played_at, sb, bb, position, table_size,
                  hero_stack, hero_cards, all_in, showdown, hero_won, invested,
                  collected, net, board, raw)
             SELECT * FROM unnest(
                 $1::text[], $2::text[], $3::text[], $4::int[], $5::int[],
                 $6::text[], $7::int[], $8::int[], $9::text[], $10::bool[],
                 $11::bool[], $12::bool[], $13::int[], $14::int[], $15::int[],
                 $16::text[], $17::text[])
             ON CONFLICT (hand_id) DO NOTHING
             RETURNING hand_id",
        )
        .bind(&ids)
        .bind(&tournament_ids)
        .bind(&played_ats)
        .bind(&sbs)
        .bind(&bbs)
        .bind(&positions)
        .bind(&table_sizes)
        .bind(&hero_stacks)
        .bind(&hero_cards)
        .bind(&all_ins)
        .bind(&showdowns)
        .bind(&hero_wons)
        .bind(&investeds)
        .bind(&collecteds)
        .bind(&nets)
        .bind(&boards)
        .bind(&raws)
        .fetch_all(&mut *transaction)
        .await?
        .into_iter()
        .collect()
    };

    transaction.commit().await?;

    let hands_new = new_hand_ids.len();
    let new_stats = aggregate_hands(&hands, Some(&new_hand_ids));

    Ok(ImportOutcome {
        zips: run.zips,
        files: run.files.len(),
        hands_parsed,
        tournaments_parsed,
        hands_new,
        hands_skipped: hands_parsed - hands_new,
        tournaments_new,
        failures: run.failures.clone(),
        new_stats,
    })
}

/// Aggregation state for one tournament while an import runs: hand timing,
/// names, and the summary-file money/place fields.
struct TournamentBuild {
    id: String,
    name: String,
    game: Option<String>,
    started: String,
    finished: Option<String>,
    buy_in_cents: Option<i32>,
    prize_cents: Option<i32>,
    place: Option<i32>,
    entrants: Option<i32>,
}

impl TournamentBuild {
    fn from_hand(hand: &ParsedHand) -> Self {
        Self {
            id: hand.tournament_id.clone(),
            name: hand.tournament_name.clone(),
            game: hand.game_type.clone(),
            started: hand.played_at.clone(),
            finished: Some(hand.played_at.clone()),
            buy_in_cents: None,
            prize_cents: None,
            place: None,
            entrants: None,
        }
    }

    fn from_summary(summary: &ParsedTournament) -> Self {
        Self {
            id: summary.id.clone(),
            name: summary.name.clone(),
            game: summary.game_type.clone(),
            started: summary
                .started
                .clone()
                .unwrap_or_else(|| "1970-01-01 00:00:00".to_string()),
            finished: None,
            buy_in_cents: summary.buy_in_cents,
            prize_cents: summary.prize_cents,
            place: summary.place,
            entrants: summary.entrants,
        }
    }

    fn merge_hand(&mut self, hand: &ParsedHand) {
        if hand.played_at < self.started {
            self.started = hand.played_at.clone();
        }
        self.finished = Some(self.finished.clone().map_or_else(
            || hand.played_at.clone(),
            |current| {
                if hand.played_at > current {
                    hand.played_at.clone()
                } else {
                    current
                }
            },
        ));
    }

    fn merge_summary(&mut self, summary: &ParsedTournament) {
        self.buy_in_cents = summary.buy_in_cents.or(self.buy_in_cents);
        self.prize_cents = summary.prize_cents.or(self.prize_cents);
        self.place = summary.place.or(self.place);
        self.entrants = summary.entrants.or(self.entrants);
        if let Some(started) = &summary.started
            && (started < &self.started || self.started == "1970-01-01 00:00:00")
        {
            self.started = started.clone();
        }
    }
}

/// Lifetime aggregates across everything imported.
pub async fn overall_stats(pool: &PgPool) -> Result<OverallStats> {
    let row: (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT
             (SELECT count(*) FROM gg_tournaments),
             (SELECT count(*) FROM gg_tournaments WHERE place = 1),
             (SELECT count(*) FROM gg_hands),
             COALESCE((SELECT sum(CASE WHEN hero_won THEN 1 ELSE 0 END) FROM gg_hands), 0),
             COALESCE((SELECT sum(CASE WHEN showdown THEN 1 ELSE 0 END) FROM gg_hands), 0),
             COALESCE((SELECT sum(CASE WHEN all_in THEN 1 ELSE 0 END) FROM gg_hands), 0),
             COALESCE((SELECT sum(buy_in_cents) FROM gg_tournaments), 0),
             COALESCE((SELECT sum(prize_cents) FROM gg_tournaments), 0),
             COALESCE((SELECT sum(invested) FROM gg_hands), 0),
             COALESCE((SELECT sum(collected) FROM gg_hands), 0),
             COALESCE((SELECT sum(net) FROM gg_hands), 0)",
    )
    .fetch_one(pool)
    .await?;
    Ok(OverallStats {
        tournaments: row.0,
        tournaments_won: row.1,
        hands: row.2,
        hands_won: row.3,
        showdowns: row.4,
        all_ins: row.5,
        buy_in_cents: row.6,
        prize_cents: row.7,
        invested: row.8,
        collected: row.9,
        net_chips: row.10,
    })
}

type ListingRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
    Option<i32>,
    i64,
    i64,
    i64,
    i64,
    i64,
);

/// Every imported tournament with its hand aggregates, newest first.
pub async fn list_tournaments(pool: &PgPool) -> Result<Vec<TournamentListing>> {
    let rows: Vec<ListingRow> = sqlx::query_as(
        "SELECT
             t.id, t.name, t.game_type, t.started_at, t.finished_at,
             t.buy_in_cents, t.prize_cents, t.place, t.entrants,
             count(h.hand_id),
             COALESCE(sum(CASE WHEN h.hero_won THEN 1 ELSE 0 END), 0),
             COALESCE(sum(CASE WHEN h.all_in THEN 1 ELSE 0 END), 0),
             COALESCE(sum(CASE WHEN h.showdown THEN 1 ELSE 0 END), 0),
             COALESCE(sum(h.net), 0)
         FROM gg_tournaments t
         LEFT JOIN gg_hands h ON h.tournament_id = t.id
         GROUP BY t.id
         ORDER BY COALESCE(t.finished_at, t.started_at) DESC, t.started_at DESC, t.id DESC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                id,
                name,
                game_type,
                started,
                finished,
                buy_in_cents,
                prize_cents,
                place,
                entrants,
                hands,
                hands_won,
                all_ins,
                showdowns,
                net_chips,
            )| TournamentListing {
                tournament: TournamentSummary {
                    id,
                    name,
                    game_type,
                    started,
                    finished,
                    buy_in_cents,
                    prize_cents,
                    place,
                    entrants,
                },
                hands,
                hands_won,
                all_ins,
                showdowns,
                net_chips,
            },
        )
        .collect())
}

type HandRowSql = (
    String,
    String,
    i32,
    i32,
    String,
    i32,
    Option<i32>,
    Option<String>,
    bool,
    bool,
    bool,
    i32,
    i32,
    i32,
    Option<String>,
);

/// One imported tournament's stored summary, aggregates, and hands (newest
/// first), or `None` when the id is unknown.
pub async fn load_tournament(
    pool: &PgPool,
    tournament_id: &str,
) -> Result<Option<TournamentDetail>> {
    let listings = list_tournaments(pool).await?;
    let listing = listings
        .into_iter()
        .find(|listing| listing.tournament.id == tournament_id);
    let Some(listing) = listing else {
        return Ok(None);
    };

    let rows: Vec<HandRowSql> = sqlx::query_as(
        "SELECT hand_id, played_at, sb, bb, position, table_size, hero_stack,
                hero_cards, all_in, showdown, hero_won, invested, collected,
                net, board
         FROM gg_hands
         WHERE tournament_id = $1
         ORDER BY played_at DESC, hand_id DESC",
    )
    .bind(tournament_id)
    .fetch_all(pool)
    .await?;

    Ok(Some(TournamentDetail {
        listing,
        hands: rows
            .into_iter()
            .map(
                |(
                    hand_id,
                    played_at,
                    sb,
                    bb,
                    position,
                    table_size,
                    hero_stack,
                    hero_cards,
                    all_in,
                    showdown,
                    hero_won,
                    invested,
                    collected,
                    net,
                    board,
                )| HandRow {
                    hand_id,
                    played_at,
                    sb,
                    bb,
                    position,
                    table_size,
                    hero_stack,
                    hero_cards,
                    all_in,
                    showdown,
                    hero_won,
                    invested,
                    collected,
                    net,
                    board,
                },
            )
            .collect(),
    }))
}

/// How many recent tournaments a fresh drill's starting stack is sampled
/// from: the modal starting stack of the hero's newest hand-history imports.
pub const STARTING_STACK_WINDOW: usize = 11;

/// The most common starting stack among the hero's `window` newest
/// tournaments. Each tournament's starting stack is the hero stack of its
/// earliest hand; the modal value wins, with ties broken toward the newest
/// tournament. `None` when the recent history holds no usable stacks.
pub async fn modal_starting_stack(pool: &PgPool, window: usize) -> Result<Option<u32>> {
    let rows: Vec<(String, Option<i32>, String)> = sqlx::query_as(
        "SELECT h.tournament_id, h.hero_stack, h.played_at
         FROM gg_hands h
         JOIN (
             SELECT id FROM gg_tournaments ORDER BY started_at DESC LIMIT $1
         ) recent ON recent.id = h.tournament_id
         ORDER BY h.tournament_id, h.played_at",
    )
    .bind(window as i64)
    .fetch_all(pool)
    .await?;

    // The rows arrive grouped by tournament and ordered by hand time, so the
    // first row of every tournament is its earliest hand.
    let mut candidates: Vec<(Option<i32>, String)> = Vec::new();
    let mut last: Option<String> = None;
    for (tournament, stack, played_at) in rows {
        if last.as_deref() != Some(tournament.as_str()) {
            last = Some(tournament);
            candidates.push((stack, played_at));
        }
    }

    let usable: Vec<(i32, String)> = candidates
        .into_iter()
        .filter_map(|(stack, played_at)| {
            stack
                .filter(|stack| *stack >= 1)
                .map(|stack| (stack, played_at))
        })
        .collect();
    Ok(pick_modal_starting_stack(&usable))
}

/// Picks the modal starting stack from one candidate per tournament
/// (`(starting stack, start time)`); ties break toward the newest tournament.
fn pick_modal_starting_stack(candidates: &[(i32, String)]) -> Option<u32> {
    let mut counts: HashMap<i32, usize> = HashMap::new();
    for (stack, _) in candidates {
        *counts.entry(*stack).or_insert(0) += 1;
    }
    let best = counts.values().copied().max()?;

    let mut newest_first: Vec<&(i32, String)> = candidates.iter().collect();
    newest_first.sort_by(|a, b| b.1.cmp(&a.1));
    newest_first
        .into_iter()
        .find(|(stack, _)| counts.get(stack) == Some(&best))
        .map(|(stack, _)| *stack as u32)
}

/// Formats stored cent amounts as dollars, e.g. `75` → `$0.75`, `-125` →
/// `-$1.25`.
pub fn money(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}${}.{:02}", cents.abs() / 100, cents.abs() % 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    // ------------------------------------------------------------ samples

    /// A real GGPoker hand block (win at showdown).
    const SAMPLE_WIN: &str = "Poker Hand #SG4176965290: Tournament #307865587, Spin&Gold #7 Hold'em No Limit - Level3(20/40) - 2026/08/21 15:07:44
Table '39856' 3-max Seat #3 is the button
Seat 2: Hero (525 in chips)
Seat 3: 14c11a2a (375 in chips)
14c11a2a: posts small blind 20
Hero: posts big blind 40
*** HOLE CARDS ***
Dealt to Hero [As Kh]
Dealt to 14c11a2a 
14c11a2a: calls 20
Hero: raises 40 to 80
14c11a2a: calls 40
*** FLOP *** [Jd 3c 8c]
Hero: bets 40
14c11a2a: calls 40
*** TURN *** [Jd 3c 8c] [Qd]
Hero: bets 40
14c11a2a: calls 40
*** RIVER *** [Jd 3c 8c Qd] [7s]
Hero: bets 40
14c11a2a: raises 175 to 215 and is all-in
Hero: calls 175
14c11a2a: shows [4d Td] (Queen high)
Hero: shows [As Kh] (Ace high)
*** SHOWDOWN ***
Hero collected 750 from pot
*** SUMMARY ***
Total pot 750 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Board [Jd 3c 8c Qd 7s]
Seat 2: Hero (big blind) showed [As Kh] and won (750) with Ace high
Seat 3: 14c11a2a (small blind) showed [4d Td] and lost with Queen high";

    /// A real GGPoker hand block (fold on the small blind).
    const SAMPLE_FOLD_SB: &str = "Poker Hand #SG4176965222: Tournament #307865587, Spin&Gold #7 Hold'em No Limit - Level3(20/40) - 2026/08/21 15:07:36
Table '39856' 3-max Seat #2 is the button
Seat 2: Hero (540 in chips)
Seat 3: 14c11a2a (360 in chips)
Hero: posts small blind 15
14c11a2a: posts big blind 30
*** HOLE CARDS ***
Dealt to Hero [6h 8d]
Hero: folds
Uncalled bet (15) returned to 14c11a2a
*** SHOWDOWN ***
14c11a2a collected 30 from pot
*** SUMMARY ***
Total pot 30 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Seat 2: Hero (small blind) folded before Flop
Seat 3: 14c11a2a (big blind) collected (30)";

    /// A real GGPoker hand block (win by betting and opponent folding, with
    /// an uncalled return to the hero).
    const SAMPLE_BLUFF_WIN: &str = "Poker Hand #SG4176963213: Tournament #307865587, Spin&Gold #7 Hold'em No Limit - Level1(10/20) - 2026/08/21 15:04:30
Table '39856' 3-max Seat #2 is the button
Seat 2: Hero (300 in chips)
Seat 3: 14c11a2a (600 in chips)
Hero: posts small blind 10
14c11a2a: posts big blind 20
*** HOLE CARDS ***
Dealt to Hero [3c Ac]
Hero: raises 20 to 40
14c11a2a: calls 20
*** FLOP *** [Ks 4s 3d]
14c11a2a: checks
Hero: bets 20
14c11a2a: calls 20
*** TURN *** [Ks 4s 3d] [6s]
14c11a2a: bets 120
Hero: calls 120
*** RIVER *** [Ks 4s 3d 6s] [2s]
14c11a2a: checks
Hero: bets 120 and is all-in
14c11a2a: folds
Uncalled bet (120) returned to Hero
*** SHOWDOWN ***
Hero collected 360 from pot
*** SUMMARY ***
Total pot 360 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Board [Ks 4s 3d 6s 2s]
Seat 2: Hero (small blind) won (360)
Seat 3: 14c11a2a (big blind) folded on the River";

    /// A real GGPoker tournament-summary block.
    const SAMPLE_SUMMARY: &str = "Tournament #307865587, Spin&Gold #7, Hold'em No Limit
Buy-in: $0.25
3 Players
Total Prize Pool: $0.75
Tournament started 2026/08/21 15:03:37 
1st : Hero, $0.75
You finished in 1st place.";

    #[test]
    fn parses_a_winning_showdown_hand() {
        let hand = parse_hand(SAMPLE_WIN).expect("a winner parses");
        assert_eq!(hand.hand_id, "SG4176965290");
        assert_eq!(hand.tournament_id, "307865587");
        assert_eq!(hand.tournament_name, "Spin&Gold #7");
        assert_eq!(hand.game_type.as_deref(), Some("Hold'em No Limit"));
        assert_eq!(hand.sb, 20);
        assert_eq!(hand.bb, 40);
        assert_eq!(hand.played_at, "2026-08-21 15:07:44");
        assert_eq!(hand.position, "BB");
        assert_eq!(hand.table_size, 2);
        assert_eq!(hand.hero_stack, Some(525));
        assert_eq!(hand.hero_cards.as_deref(), Some("As Kh"));
        assert_eq!(hand.invested, 375);
        assert_eq!(hand.collected, 750);
        assert_eq!(hand.net, 375);
        assert!(hand.hero_won);
        assert!(hand.showdown);
        assert!(!hand.all_in, "the opponent was the one all-in");
        assert_eq!(hand.board.as_deref(), Some("Jd 3c 8c Qd 7s"));
        assert!(hand.raw.contains("Poker Hand #SG4176965290"));
    }

    #[test]
    fn parses_a_small_blind_fold() {
        let hand = parse_hand(SAMPLE_FOLD_SB).expect("a fold parses");
        assert_eq!(hand.position, "SB");
        assert_eq!(hand.hero_stack, Some(540));
        assert_eq!(hand.invested, 15);
        assert_eq!(hand.collected, 0);
        assert_eq!(hand.net, -15);
        assert!(!hand.hero_won);
        assert!(!hand.showdown);
        assert!(!hand.all_in);
        assert_eq!(hand.board, None);
        assert_eq!(hand.hero_cards.as_deref(), Some("6h 8d"));
    }

    #[test]
    fn parses_a_bluff_win_with_an_uncalled_return() {
        let hand = parse_hand(SAMPLE_BLUFF_WIN).expect("a bluff win parses");
        assert_eq!(hand.position, "SB");
        assert_eq!(hand.hero_stack, Some(300));
        assert_eq!(hand.invested, 180);
        assert_eq!(hand.collected, 360);
        assert_eq!(hand.net, 180);
        assert!(hand.hero_won);
        assert!(!hand.showdown, "no cards were shown");
        assert!(hand.all_in);
        assert_eq!(hand.board.as_deref(), Some("Ks 4s 3d 6s 2s"));
    }

    #[test]
    fn parses_multiple_hands_out_of_one_file() {
        let text = format!("{SAMPLE_WIN}\n\n\n{SAMPLE_FOLD_SB}\n\n\n{SAMPLE_BLUFF_WIN}");
        let hands = parse_hands(&text);
        assert_eq!(
            hands
                .iter()
                .map(|hand| hand.hand_id.as_str())
                .collect::<Vec<_>>(),
            vec!["SG4176965290", "SG4176965222", "SG4176963213"]
        );
    }

    // --------------------------------------------------------- episodes

    #[test]
    fn episode_parses_seats_button_and_timeline() {
        let episode = parse_episode(SAMPLE_WIN).expect("sample win has an episode");
        assert_eq!(
            episode.seats,
            vec![
                EpisodeSeat {
                    no: 2,
                    name: "Hero".to_string(),
                    stack: Some(525),
                },
                EpisodeSeat {
                    no: 3,
                    name: "14c11a2a".to_string(),
                    stack: Some(375),
                },
            ]
        );
        assert_eq!(episode.button, Some(3));
        assert_eq!(
            episode.hero_cards,
            Some(["As".to_string(), "Kh".to_string()])
        );
        assert_eq!(
            episode.boards,
            vec![
                (
                    1,
                    vec!["Jd".to_string(), "3c".to_string(), "8c".to_string()]
                ),
                (
                    2,
                    vec![
                        "Jd".to_string(),
                        "3c".to_string(),
                        "8c".to_string(),
                        "Qd".to_string()
                    ]
                ),
                (
                    3,
                    vec![
                        "Jd".to_string(),
                        "3c".to_string(),
                        "8c".to_string(),
                        "Qd".to_string(),
                        "7s".to_string()
                    ]
                ),
            ]
        );
        assert_eq!(
            episode.summary_board,
            Some(vec![
                "Jd".to_string(),
                "3c".to_string(),
                "8c".to_string(),
                "Qd".to_string(),
                "7s".to_string()
            ])
        );
        let verbs: Vec<(u8, EpisodeVerb, Option<i32>, Option<i32>, bool)> = episode
            .actions
            .iter()
            .map(|action| {
                (
                    action.seat_no,
                    action.verb,
                    action.amount,
                    action.to,
                    action.all_in,
                )
            })
            .collect();
        assert_eq!(
            verbs,
            vec![
                (3, EpisodeVerb::Post, Some(20), None, false),
                (2, EpisodeVerb::Post, Some(40), None, false),
                (3, EpisodeVerb::Call, Some(20), None, false),
                (2, EpisodeVerb::Raise, Some(40), Some(80), false),
                (3, EpisodeVerb::Call, Some(40), None, false),
                (2, EpisodeVerb::Bet, Some(40), None, false),
                (3, EpisodeVerb::Call, Some(40), None, false),
                (2, EpisodeVerb::Bet, Some(40), None, false),
                (3, EpisodeVerb::Call, Some(40), None, false),
                (2, EpisodeVerb::Bet, Some(40), None, false),
                (3, EpisodeVerb::Raise, Some(175), Some(215), true),
                (2, EpisodeVerb::Call, Some(175), None, false),
            ]
        );
    }

    #[test]
    fn episode_parses_fold_and_blind_post_hands() {
        let episode = parse_episode(SAMPLE_FOLD_SB).expect("fold sample has an episode");
        assert_eq!(episode.button, Some(2));
        assert_eq!(
            episode.hero_cards,
            Some(["6h".to_string(), "8d".to_string()])
        );
        assert!(episode.boards.is_empty(), "the hand ended preflop");
        assert_eq!(episode.summary_board, None);
        assert_eq!(
            episode.actions,
            vec![
                EpisodeAction {
                    seat_no: 2,
                    verb: EpisodeVerb::Post,
                    amount: Some(15),
                    to: None,
                    all_in: false,
                },
                EpisodeAction {
                    seat_no: 3,
                    verb: EpisodeVerb::Post,
                    amount: Some(30),
                    to: None,
                    all_in: false,
                },
                EpisodeAction {
                    seat_no: 2,
                    verb: EpisodeVerb::Fold,
                    amount: None,
                    to: None,
                    all_in: false,
                },
            ]
        );
    }

    #[test]
    fn episode_seats_tolerate_missing_stacks() {
        let text = "Poker Hand #SG1: Tournament #9, Spin&Gold #1 Hold'em No Limit - Level1(10/20) - 2026/08/21 15:03:55
Table '1' 2-max Seat #2 is the button
Seat 2: Hero
Seat 3: 14c11a2a (310 in chips)
Hero: posts small blind 10
14c11a2a: posts big blind 20
*** HOLE CARDS ***
Dealt to Hero [As Ks]
Hero: raises 20 to 40
14c11a2a: folds
Uncalled bet (20) returned to Hero
*** SHOWDOWN ***
Hero collected 40 from pot
*** SUMMARY ***
Seat 2: Hero collected (40)";
        let episode = parse_episode(text).expect("seat without stack still parses");
        assert_eq!(episode.seats[0].stack, None);
        assert_eq!(episode.seats[1].stack, Some(310));
        assert_eq!(episode.actions.len(), 4);
    }

    #[test]
    fn episode_parses_stored_raw_blocks_without_the_prefix() {
        // `parse_hands` stores every block without the `Poker Hand #`
        // prefix; the episode parser must accept both shapes.
        let stripped = SAMPLE_WIN.strip_prefix("Poker Hand #").unwrap();
        assert!(!stripped.starts_with("Poker Hand #"));
        let episode = parse_episode(stripped).expect("prefix-less raw parses");
        assert_eq!(
            episode.actions.len(),
            parse_episode(SAMPLE_WIN).unwrap().actions.len()
        );
        assert_eq!(episode.button, Some(3));
        assert_eq!(
            episode.seats,
            vec![
                EpisodeSeat {
                    no: 2,
                    name: "Hero".to_string(),
                    stack: Some(525),
                },
                EpisodeSeat {
                    no: 3,
                    name: "14c11a2a".to_string(),
                    stack: Some(375),
                },
            ]
        );
    }

    #[test]
    fn unrecognizable_blocks_have_no_episode() {
        assert_eq!(parse_episode("garbage"), None);
    }

    #[test]
    fn episode_card_helpers_cover_bracket_shapes() {
        assert_eq!(
            cards_in_brackets("[Jd 3c 8c] [Qd] [7s]"),
            vec!["Jd", "3c", "8c", "Qd", "7s"]
        );
        assert_eq!(
            two_cards(&["As".to_string(), "Kh".to_string()]),
            Some(["As".to_string(), "Kh".to_string()])
        );
        assert_eq!(two_cards(&["As".to_string()]), None);
        assert_eq!(
            parse_button_line("Table '1' 3-max Seat #2 is the button"),
            Some(2)
        );
        assert_eq!(parse_button_line("no button here"), None);
        assert_eq!(
            parse_street_marker("*** FLOP *** [Jd 3c 8c]"),
            Some((1, "[Jd 3c 8c]".to_string()))
        );
        assert_eq!(parse_street_marker("*** HOLE CARDS ***"), None);
    }

    #[test]
    fn skips_malformed_blocks() {
        let garbage = "Poker Hand #X: not a handle\nnonsense\nlines";
        assert!(parse_hand(garbage).is_none());
        assert_eq!(parse_hands("no content at all"), Vec::<ParsedHand>::new());
    }

    #[test]
    fn parses_a_three_max_hand_without_a_hero_post() {
        let text = "Poker Hand #SG4176962837: Tournament #307865587, Spin&Gold #7 Hold'em No Limit - Level1(10/20) - 2026/08/21 15:03:55
Table '39856' 3-max Seat #2 is the button
Seat 1: facf7b06 (300 in chips)
Seat 2: Hero (290 in chips)
Seat 3: 14c11a2a (310 in chips)
14c11a2a: posts small blind 10
facf7b06: posts big blind 20
*** HOLE CARDS ***
Dealt to facf7b06 
Dealt to Hero [4c 7s]
Hero: folds
14c11a2a: raises 40 to 60
facf7b06: calls 40
*** FLOP *** [7c 6d 2h]
facf7b06: raises 150 to 240 and is all-in
14c11a2a: calls 150
facf7b06: shows [Ks 5s] (King high)
14c11a2a: shows [Qs Ad] (Ace high)
*** TURN *** [7c 6d 2h] [8s]
*** RIVER *** [7c 6d 2h 8s] [Qh]
*** SHOWDOWN ***
14c11a2a collected 600 from pot
*** SUMMARY ***
Total pot 600 | Rake 0 | Jackpot 0 | Bingo 0 | Fortune 0 | Tax 0
Board [7c 6d 2h 8s Qh]
Seat 1: facf7b06 (big blind) showed [Ks 5s] and lost with King high
Seat 2: Hero (button) folded before Flop (didn't bet)
Seat 3: 14c11a2a (small blind) showed [Qs Ad] and won (600) with a pair of Queens";
        let hand = parse_hand(text).expect("a 3-max hand parses");
        assert_eq!(hand.table_size, 3);
        assert_eq!(hand.position, "BTN");
        assert_eq!(hand.invested, 0);
        assert_eq!(hand.net, 0);
        assert!(!hand.hero_won);
        assert!(!hand.showdown);
    }

    #[test]
    fn parses_a_tournament_summary() {
        let summary = parse_tournament_summary(SAMPLE_SUMMARY).expect("summary parses");
        assert_eq!(summary.id, "307865587");
        assert_eq!(summary.name, "Spin&Gold #7");
        assert_eq!(summary.game_type.as_deref(), Some("Hold'em No Limit"));
        assert_eq!(summary.buy_in_cents, Some(25));
        assert_eq!(summary.prize_cents, Some(75));
        assert_eq!(summary.place, Some(1));
        assert_eq!(summary.entrants, Some(3));
        assert_eq!(summary.started.as_deref(), Some("2026-08-21 15:03:37"));
    }

    #[test]
    fn unrecognizable_summaries_are_none() {
        assert_eq!(parse_tournament_summary("hello world"), None);
    }

    #[test]
    fn file_content_switches_on_the_first_line() {
        let hands = parse_file_content(SAMPLE_WIN);
        assert_eq!(hands.hands.len(), 1);
        assert_eq!(hands.tournament, None);

        let summary = parse_file_content(SAMPLE_SUMMARY);
        assert!(summary.hands.is_empty());
        assert!(summary.tournament.is_some());
    }

    #[test]
    fn header_edge_cases_fail_softly() {
        assert!(parse_header("not a header").is_none());
        assert_eq!(split_name_game("Cash Game No Limit").0, "Cash Game");
        assert_eq!(split_name_game("Spin&Gold #7").1, None);
        assert_eq!(normalize_date("2026/08/21 15:07:44"), "2026-08-21 15:07:44");
    }

    #[test]
    fn blind_seat_parsing_tolerates_missing_stacks() {
        let text = "Poker Hand #SG1: Tournament #9, Spin&Gold #7 Hold'em No Limit - Level1(10/20) - 2026/08/21 15:03:55
Table '1' 3-max Seat #2 is the button
Seat 2: Hero
Seat 3: 14c11a2a (310 in chips)
Hero: posts small blind 10
14c11a2a: posts big blind 20
*** HOLE CARDS ***
Dealt to Hero [As Ks]
Hero: calls 10
14c11a2a: checks
*** FLOP *** [2c 7h 9d]
14c11a2a: bets 20
Hero: folds
*** SUMMARY ***
Board [2c 7h 9d]
Seat 3: 14c11a2a collected";
        let hand = parse_hand(text).expect("seat without stack still parses");
        assert_eq!(hand.hero_stack, None);
        assert_eq!(hand.position, "SB");
        assert_eq!(hand.invested, 20);
        assert!(!hand.showdown);
    }

    #[test]
    fn commitments_cover_every_action_kind() {
        assert_eq!(hero_commitment("posts small blind 20", 0), 20);
        assert_eq!(hero_commitment("posts big blind 40", 0), 40);
        assert_eq!(hero_commitment("calls 20", 30), 20);
        assert_eq!(hero_commitment("calls 175 and is all-in", 200), 175);
        assert_eq!(hero_commitment("bets 40", 80), 40);
        // Raises name the total commitment: the increment is `to - invested`.
        assert_eq!(hero_commitment("raises 40 to 80", 40), 40);
        assert_eq!(hero_commitment("raises 20 to 40", 10), 30);
        assert_eq!(hero_commitment("raises 175 to 215 and is all-in", 200), 15);
        assert_eq!(hero_commitment("checks", 40), 0);
        assert_eq!(hero_commitment("folds", 40), 0);
    }

    #[test]
    fn cents_parse_tolerates_whitespace() {
        assert_eq!(parse_cents("0.25"), Some(25));
        assert_eq!(parse_cents(" 1.25 "), Some(125));
        assert_eq!(parse_cents("nope"), None);
        assert_eq!(money(75), "$0.75");
        assert_eq!(money(0), "$0.00");
        assert_eq!(money(-125), "-$1.25");
        assert_eq!(money(12340), "$123.40");
    }

    #[test]
    fn aggregating_hands_counts_wins_and_chips() {
        let hands = vec![
            parse_hand(SAMPLE_WIN).unwrap(),
            parse_hand(SAMPLE_FOLD_SB).unwrap(),
            parse_hand(SAMPLE_BLUFF_WIN).unwrap(),
        ];
        let stats = aggregate_hands(&hands, None);
        assert_eq!(stats.hands, 3);
        assert_eq!(stats.won, 2);
        assert_eq!(stats.lost, 1);
        assert!((stats.win_ratio - 66.666).abs() < 0.01);
        assert_eq!(stats.all_ins, 1, "only the river bluff went all-in");
        assert_eq!(
            stats.showdowns, 1,
            "only the revealed hand reached showdown"
        );
        assert_eq!(stats.invested, 570);
        assert_eq!(stats.collected, 1110);
        assert_eq!(stats.net_chips, 540);
        assert_eq!(stats.tournaments, 1);

        let only_new: std::collections::HashSet<String> =
            ["SG4176963213".to_string()].into_iter().collect();
        let new = aggregate_hands(&hands, Some(&only_new));
        assert_eq!(new.hands, 1);
        assert_eq!(new.won, 1);
        assert_eq!(new.net_chips, 180);
    }

    #[test]
    fn modal_starting_stack_picks_the_mode_and_prefers_newest_on_ties() {
        assert_eq!(pick_modal_starting_stack(&[]), None);

        let single = [(300, "2026-08-21 10:00:00".to_string())];
        assert_eq!(pick_modal_starting_stack(&single), Some(300));

        let mode = [
            (300, "2026-08-20 10:00:00".to_string()),
            (500, "2026-08-21 10:00:00".to_string()),
            (300, "2026-08-22 10:00:00".to_string()),
        ];
        assert_eq!(pick_modal_starting_stack(&mode), Some(300));

        let tie = [
            (300, "2026-08-20 10:00:00".to_string()),
            (500, "2026-08-21 10:00:00".to_string()),
            (300, "2026-08-22 10:00:00".to_string()),
            (500, "2026-08-23 10:00:00".to_string()),
        ];
        assert_eq!(
            pick_modal_starting_stack(&tie),
            Some(500),
            "a tied modal falls to the newest tournament"
        );
    }

    #[test]
    fn scanning_an_empty_or_missing_directory_finds_nothing() {
        let run = scan_directory(Path::new("definitely-not-a-real-dir")).unwrap();
        assert_eq!(
            run,
            ScanRun {
                files: Vec::new(),
                failures: Vec::new(),
                zips: 0
            }
        );
    }

    #[test]
    fn default_dir_points_at_history() {
        assert_eq!(default_history_dir(), PathBuf::from("history"));
    }

    mod scanning {
        use super::*;
        use std::io::Write;
        use zip::write::SimpleFileOptions;

        fn zip_with_entry(dir: &Path, zip_name: &str, entry: &str, content: &str) {
            std::fs::create_dir_all(dir).unwrap();
            let file = std::fs::File::create(dir.join(zip_name)).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file(entry, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content.as_bytes()).unwrap();
            writer.finish().unwrap();
        }

        fn temp_dir(tag: &str) -> PathBuf {
            let dir =
                std::env::temp_dir().join(format!("pokertrainer_hh_{tag}_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        }

        #[test]
        fn scans_zips_recursively_and_parses_entries() {
            let dir = temp_dir("scan");
            zip_with_entry(&dir, "a.zip", "hands.txt", SAMPLE_WIN);
            zip_with_entry(&dir, "b.zip", "summary.txt", SAMPLE_SUMMARY);
            zip_with_entry(
                &dir.join("nested"),
                "c.zip",
                "both.txt",
                &format!("{SAMPLE_WIN}\n\n{SAMPLE_FOLD_SB}"),
            );
            let run = scan_directory(&dir).unwrap();
            assert_eq!(run.zips, 3);
            assert_eq!(run.files.len(), 3);
            assert!(run.failures.is_empty());
            assert_eq!(run.files[0].hands.len(), 1);
            assert_eq!(run.files[1].tournament.as_ref().unwrap().id, "307865587");
            assert_eq!(run.files[2].hands.len(), 2);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn unreadable_and_unrecognizable_zips_land_in_failures() {
            let dir = temp_dir("bad");
            zip_with_entry(&dir, "bad.zip", "bad.txt", "binary junk");
            std::fs::write(dir.join("garbage.zip"), b"this is not a zip").unwrap();
            let run = scan_directory(&dir).unwrap();
            assert_eq!(run.files, Vec::new());
            assert_eq!(run.failures.len(), 2);
            assert!(run.failures[0].contains("bad.zip"), "{:?}", run.failures);
            assert!(
                run.failures[1].contains("garbage.zip"),
                "{:?}",
                run.failures
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn non_txt_entries_are_ignored_without_failure() {
            let dir = temp_dir("mixed");
            zip_with_entry(&dir, "mixed.zip", "notes.md", "hello");
            zip_with_entry(&dir, "mixed.zip", "hands.txt", SAMPLE_WIN);
            let run = scan_directory(&dir).unwrap();
            assert_eq!(run.files.len(), 1);
            assert!(run.failures.is_empty());
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }

    // ----------------------------------------------------- database tests

    async fn test_pool() -> PgPool {
        crate::db::test_pool().await
    }

    fn unique_id(prefix: &str) -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}_{nanos}")
    }

    fn hand_for(tournament_id: &str, hand_id: &str, won: bool, net: i32) -> ParsedHand {
        ParsedHand {
            hand_id: hand_id.to_string(),
            tournament_id: tournament_id.to_string(),
            tournament_name: "Spin&Gold #7".to_string(),
            game_type: Some("Hold'em No Limit".to_string()),
            played_at: "2026-08-21 15:07:44".to_string(),
            sb: 10,
            bb: 20,
            position: "BB".to_string(),
            table_size: 3,
            hero_stack: Some(500),
            hero_cards: Some("As Kh".to_string()),
            all_in: net != 0,
            showdown: won,
            hero_won: won,
            invested: 100,
            collected: 100 + net,
            net,
            board: None,
            raw: format!("Poker Hand #{hand_id}"),
        }
    }

    fn run_of(hands: Vec<ParsedHand>, tournament: Option<ParsedTournament>) -> ScanRun {
        let files = vec![ScannedFile {
            zip_name: "test.zip".to_string(),
            entry_name: "hands.txt".to_string(),
            hands,
            tournament,
        }];
        ScanRun {
            files,
            failures: Vec::new(),
            zips: 1,
        }
    }

    async fn delete_tournament(pool: &PgPool, id: &str) {
        sqlx::query("DELETE FROM gg_tournaments WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn import_roundtrips_and_rescans_are_idempotent() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let before = overall_stats(&pool).await.unwrap();
        let tournament_id = unique_id("T");
        let hand_a = hand_for(&tournament_id, &unique_id("H"), true, 250);
        let hand_b = hand_for(&tournament_id, &unique_id("H"), false, -150);

        let run = run_of(
            vec![hand_a.clone(), hand_b.clone()],
            Some(ParsedTournament {
                id: tournament_id.clone(),
                name: "Spin&Gold #7".to_string(),
                game_type: Some("Hold'em No Limit".to_string()),
                started: Some("2026-08-21 15:03:37".to_string()),
                buy_in_cents: Some(25),
                prize_cents: Some(75),
                place: Some(1),
                entrants: Some(3),
            }),
        );
        let outcome = import_scan(&pool, &run).await.unwrap();
        assert_eq!(outcome.hands_parsed, 2);
        assert_eq!(outcome.hands_new, 2);
        assert_eq!(outcome.hands_skipped, 0);
        assert_eq!(outcome.tournaments_parsed, 1);
        assert_eq!(outcome.tournaments_new, 1);
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.new_stats.hands, 2);
        assert_eq!(outcome.new_stats.won, 1);
        assert!((outcome.new_stats.win_ratio - 50.0).abs() < 1e-9);
        assert_eq!(outcome.new_stats.net_chips, 100);
        assert_eq!(outcome.new_stats.tournaments, 1);

        let again = import_scan(&pool, &run).await.unwrap();
        assert_eq!(again.hands_new, 0);
        assert_eq!(again.hands_skipped, 2);
        assert_eq!(again.tournaments_new, 0);
        assert_eq!(again.new_stats.hands, 0);

        let stats = overall_stats(&pool).await.unwrap();
        assert_eq!(
            stats.tournaments,
            before.tournaments + 1,
            "one tournament per import, regardless of pre-existing data"
        );
        assert_eq!(stats.tournaments_won, before.tournaments_won + 1);
        assert_eq!(stats.hands, before.hands + 2);
        assert_eq!(stats.hands_won, before.hands_won + 1);
        assert_eq!(stats.all_ins, before.all_ins + 2);
        assert_eq!(stats.buy_in_cents, before.buy_in_cents + 25);
        assert_eq!(stats.prize_cents, before.prize_cents + 75);
        assert_eq!(stats.invested, before.invested + 200);
        assert_eq!(stats.collected, before.collected + 300);
        assert_eq!(stats.net_chips, before.net_chips + 100);

        let listing = list_tournaments(&pool).await.unwrap();
        let row = listing
            .iter()
            .find(|row| row.tournament.id == tournament_id)
            .expect("the imported tournament is listed");
        assert_eq!(row.hands, 2);
        assert_eq!(row.hands_won, 1);
        assert_eq!(row.all_ins, 2);
        assert_eq!(row.showdowns, 1);
        assert_eq!(row.net_chips, 100);
        assert_eq!(row.tournament.started, "2026-08-21 15:03:37");
        assert_eq!(
            row.tournament.finished.as_deref(),
            Some("2026-08-21 15:07:44")
        );
        assert_eq!(row.tournament.buy_in_cents, Some(25));
        assert_eq!(row.tournament.prize_cents, Some(75));
        assert_eq!(row.tournament.place, Some(1));

        let detail = load_tournament(&pool, &tournament_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.listing, row.clone());
        assert_eq!(detail.hands.len(), 2);
        assert!(detail.hands[0].played_at >= detail.hands[1].played_at);
        assert_eq!(detail.hands.iter().filter(|hand| hand.hero_won).count(), 1);

        assert_eq!(
            load_tournament(&pool, &unique_id("MISSING")).await.unwrap(),
            None
        );

        delete_tournament(&pool, &tournament_id).await;
    }

    #[tokio::test]
    async fn rescan_fills_in_summary_fields_that_arrive_later() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let tournament_id = unique_id("T");
        let hand = hand_for(&tournament_id, &unique_id("H"), true, 50);
        let hands_only = run_of(vec![hand], None);
        let first = import_scan(&pool, &hands_only).await.unwrap();
        assert_eq!(first.tournaments_new, 1);

        let before = list_tournaments(&pool).await.unwrap();
        let before = before
            .iter()
            .find(|l| l.tournament.id == tournament_id)
            .unwrap();
        assert_eq!(before.tournament.buy_in_cents, None);
        assert_eq!(before.tournament.place, None);

        let with_summary = run_of(
            Vec::new(),
            Some(ParsedTournament {
                id: tournament_id.clone(),
                name: "Spin&Gold #7".to_string(),
                game_type: Some("Hold'em No Limit".to_string()),
                started: Some("2026-08-21 15:03:37".to_string()),
                buy_in_cents: Some(25),
                prize_cents: Some(75),
                place: Some(1),
                entrants: Some(3),
            }),
        );
        import_scan(&pool, &with_summary).await.unwrap();

        let after = list_tournaments(&pool).await.unwrap();
        let after = after
            .iter()
            .find(|l| l.tournament.id == tournament_id)
            .unwrap();
        assert_eq!(after.tournament.buy_in_cents, Some(25));
        assert_eq!(after.tournament.prize_cents, Some(75));
        assert_eq!(after.tournament.place, Some(1));
        assert_eq!(after.tournament.entrants, Some(3));
        assert_eq!(after.tournament.started, "2026-08-21 15:03:37");

        delete_tournament(&pool, &tournament_id).await;
    }

    #[tokio::test]
    async fn listings_run_newest_first() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        // Future dates keep the created rows ahead of any pre-existing data.
        let older = unique_id("T");
        let newer = unique_id("T");
        let mut early = hand_for(&older, &unique_id("H"), true, 10);
        early.played_at = "2099-01-01 10:00:00".to_string();
        import_scan(&pool, &run_of(vec![early], None))
            .await
            .unwrap();

        let mut late = hand_for(&newer, &unique_id("H"), false, -10);
        late.played_at = "2099-01-02 10:00:00".to_string();
        import_scan(&pool, &run_of(vec![late], None)).await.unwrap();

        let listing = list_tournaments(&pool).await.unwrap();
        let newest: Vec<String> = listing
            .iter()
            .take(2)
            .map(|row| row.tournament.id.clone())
            .collect();
        assert_eq!(newest, vec![newer.clone(), older.clone()]);

        delete_tournament(&pool, &older).await;
        delete_tournament(&pool, &newer).await;
    }

    #[tokio::test]
    async fn import_against_a_closed_pool_fails() {
        let pool = test_pool().await;
        pool.close().await;
        let empty = ScanRun {
            files: Vec::new(),
            failures: Vec::new(),
            zips: 0,
        };
        let err = import_scan(&pool, &empty).await.unwrap_err();
        assert!(matches!(err, Error::Sqlx(_)));
    }

    /// Seeds a tournament plus hands directly (skipping the zip import) so
    /// the modal starting-stack window can be exercised exactly.
    async fn seed_drill_history(
        pool: &PgPool,
        id: &str,
        started: &str,
        hands: &[(&str, Option<i32>)],
    ) {
        sqlx::query(
            "INSERT INTO gg_tournaments (id, name, started_at)
             VALUES ($1, 'Spin&Gold #7', $2)",
        )
        .bind(id)
        .bind(started)
        .execute(pool)
        .await
        .unwrap();
        for (index, (played_at, stack)) in hands.iter().enumerate() {
            sqlx::query(
                "INSERT INTO gg_hands
                     (hand_id, tournament_id, played_at, sb, bb, position,
                      table_size, hero_stack, all_in, showdown, hero_won,
                      invested, collected, net, raw)
                 VALUES ($1, $2, $3, 10, 20, 'BB', 3, $4, false, false,
                         false, 0, 0, 0, 'raw')",
            )
            .bind(format!("{id}_h{index}"))
            .bind(id)
            .bind(played_at)
            .bind(stack)
            .execute(pool)
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn modal_starting_stack_samples_the_newest_window() {
        let _guard = crate::analytics::DB_TEST_LOCK.lock().await;
        let pool = test_pool().await;
        let tag = unique_id("M");
        let a = format!("{tag}A");
        let b = format!("{tag}B");
        let c = format!("{tag}C");
        let d = format!("{tag}D");
        let e = format!("{tag}E");

        // Five tournaments newest wins: a 300 (later hands ignored), b 500,
        // c 300, d without a readable first-hand stack, e 300.
        seed_drill_history(
            &pool,
            &a,
            "2099-02-01 10:00:00",
            &[
                ("2099-02-01 10:00:00", Some(300)),
                ("2099-02-01 10:05:00", Some(800)),
            ],
        )
        .await;
        seed_drill_history(
            &pool,
            &b,
            "2099-02-02 10:00:00",
            &[("2099-02-02 10:00:00", Some(500))],
        )
        .await;
        seed_drill_history(
            &pool,
            &c,
            "2099-02-03 10:00:00",
            &[("2099-02-03 10:00:00", Some(300))],
        )
        .await;
        seed_drill_history(
            &pool,
            &d,
            "2099-02-04 10:00:00",
            &[
                ("2099-02-04 10:00:00", None),
                ("2099-02-04 10:05:00", Some(700)),
            ],
        )
        .await;
        seed_drill_history(
            &pool,
            &e,
            "2099-02-05 10:00:00",
            &[("2099-02-05 10:00:00", Some(300))],
        )
        .await;

        assert_eq!(
            modal_starting_stack(&pool, 2).await.unwrap(),
            Some(300),
            "the newest two tournaments leave only e's readable 300"
        );
        assert_eq!(
            modal_starting_stack(&pool, 5).await.unwrap(),
            Some(300),
            "three 300s beat the lone 500"
        );

        delete_tournament(&pool, &c).await;
        delete_tournament(&pool, &e).await;
        assert_eq!(
            modal_starting_stack(&pool, 5).await.unwrap(),
            Some(500),
            "a 300/500 tie falls to the newer tournament (b over a)"
        );

        delete_tournament(&pool, &a).await;
        delete_tournament(&pool, &b).await;
        delete_tournament(&pool, &d).await;
    }
}
