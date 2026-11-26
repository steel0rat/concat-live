// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Concat Live contributors

//! Translated presentation of bundled effect metadata. IDs stay with the engine.

use std::collections::HashMap;
use std::sync::OnceLock;

type Words = (&'static str, &'static str);

fn translations() -> &'static HashMap<&'static str, Words> {
    const TEXT: &str = include_str!("../locales/effects-ru.tsv");
    static WORDS: OnceLock<HashMap<&'static str, Words>> = OnceLock::new();
    WORDS.get_or_init(|| {
        TEXT.lines()
            .filter_map(|line| {
                let (key, words) = line.split_once('\t')?;
                Some((key, words.split_once('\t')?))
            })
            .collect()
    })
}

fn ru(id: &str) -> Option<Words> {
    if crate::i18n::current() == "ru" {
        translations().get(id.strip_prefix("concat.")?).copied()
    } else {
        None
    }
}

pub(crate) fn name<'a>(id: &str, source: &'a str) -> &'a str {
    ru(id).map_or(source, |(name, _)| name)
}

pub(crate) fn description<'a>(id: &str, source: &'a str) -> &'a str {
    ru(id).map_or(source, |(_, description)| description)
}

pub(crate) fn label(source: &str) -> &str {
    if crate::i18n::current() != "ru" {
        return source;
    }
    const TEXT: &str = include_str!("../locales/effect-controls-ru.tsv");
    static LABELS: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
    let labels = LABELS.get_or_init(|| {
        TEXT.lines()
            .filter_map(|line| line.split_once('\t'))
            .collect()
    });
    labels.get(source).copied().unwrap_or(source)
}

pub(crate) fn unit(source: &str) -> &str {
    if crate::i18n::current() != "ru" {
        return source;
    }
    match source {
        "dB" => "дБ",
        "Hz" => "Гц",
        "s" => "с",
        "ms" => "мс",
        "st" => "полут.",
        "px" => "пкс",
        "bpm" => "уд/мин",
        _ => source,
    }
}
