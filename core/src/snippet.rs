// Cuely is an open source web search engine.
// Copyright (C) 2022 Cuely ApS
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use crate::query::Query;
use crate::search_prettifier::html_escape;
use crate::spell::sentence_ranges;
use crate::tokenizer::{Stemmed, Tokenizer};
use crate::webpage::region::Region;
use std::collections::{HashMap, HashSet};
use std::ops::Range;

use itertools::Itertools;
use whatlang::Lang;

/// For now we use an algorithm similar to the `UnifiedHighlighter` in lucene <https://lucene.apache.org/core/7_3_1/highlighter/org/apache/lucene/search/uhighlight/UnifiedHighlighter.html>.
/// The document text is treated as the entire corpus, and each passage is scored as a document in this corpus using BM25.
/// The top scoring passage is used as the start of a snippet, maybe combined with the subsequent passage(s) in order to
/// reach the desired snippet length.
///
/// In the future we want to implement something closer to the method described in <https://cs.pomona.edu/~dkauchak/ir_project/whitepapers/Snippet-IL.pdf>.
/// This might require us to store each paragraph of the webpage separately to get adequate performance (maybe we can split passages online with adequate performance
/// but we need to test this).

const DESIRED_NUM_CHARS: usize = 225;
const DELTA_NUM_CHARS: usize = 25;
const MIN_PASSAGE_WIDTH: usize = 20;

const K1: f64 = 1.2;
const B: f64 = 0.75;

#[derive(Debug)]
struct PassageCandidate {
    score: f64,
    text: String,
    doc_terms: HashMap<String, u64>,
}

#[derive(Debug)]
struct SnippetString {
    fragment: String,
    highlighted: Vec<Range<usize>>,
}

const HIGHLIGHTEN_PREFIX: &str = "<b>";
const HIGHLIGHTEN_POSTFIX: &str = "</b>";

impl SnippetString {
    fn to_html(&self) -> String {
        let mut html = String::new();
        let mut start_from: usize = 0;

        for item in self.highlighted.iter() {
            if item.start < start_from {
                start_from = item.end;
                continue;
            }
            html.push_str(&html_escape(&self.fragment[start_from..item.start]));
            html.push_str(HIGHLIGHTEN_PREFIX);
            html.push_str(&html_escape(&self.fragment[item.clone()]));
            html.push_str(HIGHLIGHTEN_POSTFIX);
            start_from = item.end;
        }
        html.push_str(&html_escape(
            &self.fragment[start_from..self.fragment.len()],
        ));
        html
    }

    fn highlight(&mut self, terms: &HashSet<String>, lang: whatlang::Lang) {
        let tokenizer = Tokenizer::Stemmed(Stemmed::with_forced_language(lang));
        let mut stream = tantivy::tokenizer::Tokenizer::token_stream(&tokenizer, &self.fragment);
        while let Some(tok) = stream.next() {
            if terms.contains(&tok.text) {
                self.highlighted.push(tok.offset_from..tok.offset_to);
            }
        }
    }
}

fn snippet_string(text: &str, terms: &[String], lang: whatlang::Lang) -> SnippetString {
    let tokenizer = Tokenizer::Stemmed(Stemmed::with_forced_language(lang));

    let terms: HashSet<String> = terms
        .iter()
        .flat_map(|term| {
            let mut stream = tantivy::tokenizer::Tokenizer::token_stream(&tokenizer, term);

            let mut res = Vec::new();
            while let Some(tok) = stream.next() {
                res.push(tok.text.clone());
            }

            res.into_iter()
        })
        .collect();

    let mut passages: Vec<_> = sentence_ranges(text)
        .into_iter()
        .filter(|offset| offset.end - offset.start > MIN_PASSAGE_WIDTH)
        .map(|offset| {
            let sentence = text[offset].to_string();

            let mut doc_terms = HashMap::new();

            {
                let mut stream = tantivy::tokenizer::Tokenizer::token_stream(&tokenizer, &sentence);
                while let Some(tok) = stream.next() {
                    *doc_terms.entry(tok.text.clone()).or_insert(0) += 1;
                }
            }

            PassageCandidate {
                score: 0.0,
                text: sentence,
                doc_terms,
            }
        })
        .collect();

    if passages.is_empty() {
        let above_upper = text.len() > DESIRED_NUM_CHARS + DELTA_NUM_CHARS;

        let mut snippet = SnippetString {
            fragment: text.chars().take(DESIRED_NUM_CHARS).collect(),
            highlighted: Vec::new(),
        };

        if !snippet.fragment.is_empty() && above_upper {
            snippet.fragment += "...";
        }

        snippet.highlight(&terms, lang);

        return snippet;
    }

    let mut total_d_size = 0;

    for passage in &passages {
        total_d_size += passage.doc_terms.len();
    }

    let avg_d_size = total_d_size / passages.len();

    let mut n: HashMap<_, _> = terms.iter().map(|term| (term.to_string(), 0)).collect();

    for term in &terms {
        for passage in &passages {
            if passage.doc_terms.contains_key(term) {
                *n.entry(term.to_string()).or_insert(0) += 1;
            }
        }
    }

    let idf: HashMap<_, _> = n
        .into_iter()
        .map(|(term, freq)| {
            (
                term,
                (((passages.len() as f64) - (freq as f64) + 0.5) / ((freq as f64) + 0.5) + 1.0)
                    .ln(),
            )
        })
        .collect();

    for passage in &mut passages {
        for term in &terms {
            let f = *passage.doc_terms.get(term).unwrap_or(&0) as f64;
            passage.score += idf[term]
                * ((f * (K1 + 1.0))
                    / (f + K1
                        * (1.0 - B + B * (passage.doc_terms.len() as f64 / avg_d_size as f64))));
        }
    }

    let best_idx = passages
        .iter()
        .position_max_by(|a, b| a.score.total_cmp(&b.score))
        .expect("passages cannot be empty at this point");

    let best_passage = &passages[best_idx];
    let mut snippet = SnippetString {
        fragment: best_passage.text.clone(),
        highlighted: Vec::new(),
    };

    if snippet.fragment.len() > DESIRED_NUM_CHARS + DELTA_NUM_CHARS {
        // TODO: find 'DESIRED_NUM_CHARS' sized window that contains most highlights
        // instead of taking the prefix of the passage as a snippet
        snippet.fragment = snippet
            .fragment
            .chars()
            .take(DESIRED_NUM_CHARS + DELTA_NUM_CHARS)
            .collect();
        snippet.fragment += "...";
    } else {
        let mut next_passage_idx = best_idx + 1;

        while snippet.fragment.len() < DESIRED_NUM_CHARS - DELTA_NUM_CHARS
            && next_passage_idx < passages.len()
        {
            snippet.fragment += " ";
            snippet.fragment += &passages[next_passage_idx].text;
            next_passage_idx += 1;
        }

        if snippet.fragment.len() > DESIRED_NUM_CHARS + DELTA_NUM_CHARS {
            snippet.fragment = snippet
                .fragment
                .chars()
                .take(DESIRED_NUM_CHARS + DELTA_NUM_CHARS)
                .collect();
            snippet.fragment += "...";
        }
    }
    snippet.highlight(&terms, lang);

    snippet
}

pub fn generate(query: &Query, text: &str, region: &Region) -> String {
    let lang = match region.lang() {
        Some(lang) => lang,
        None => whatlang::detect_lang(text).unwrap_or(Lang::Eng),
    };

    let snippet = snippet_string(text, &query.simple_terms(), lang);

    snippet.to_html()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        index::Index,
        search_prettifier::Snippet,
        searcher::{LocalSearcher, SearchQuery},
        webpage::Webpage,
    };

    const TEST_TEXT: &str = r#"Rust is a systems programming language sponsored by
Mozilla which describes it as a "safe, concurrent, practical language", supporting functional and
imperative-procedural paradigms. Rust is syntactically similar to C++[according to whom?],
but its designers intend it to provide better memory safety while still maintaining
performance.
Rust is free and open-source software, released under an MIT License, or Apache License
2.0. Its designers have refined the language through the experiences of writing the Servo
web browser layout engine[14] and the Rust compiler. A large proportion of current commits
to the project are from community members.[15]
Rust won first place for "most loved programming language" in the Stack Overflow Developer
Survey in 2016, 2017, and 2018."#;

    fn snippet_text(snippet: Snippet) -> String {
        match snippet {
            Snippet::Normal { date: _, text } => text,
            _ => panic!("The snippet was not text"),
        }
    }

    #[test]
    fn snippet_during_search() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage::new(
                &format!(
                    r#"
                        <html>
                            <head>
                                <title>Website for runners</title>
                            </head>
                            <body>
                                {TEST_TEXT}
                            </body>
                        </html>
                    "#
                ),
                "https://www.example.com",
            ))
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let searcher = LocalSearcher::from(index);

        let result = searcher
            .search(&SearchQuery {
                query: "rust language".to_string(),
                ..Default::default()
            })
            .expect("Search failed");

        assert_eq!(result.num_hits, 1);
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(snippet_text(result.webpages[0].snippet.clone()), format!("{HIGHLIGHTEN_PREFIX}Rust{HIGHLIGHTEN_POSTFIX} is a systems programming {HIGHLIGHTEN_PREFIX}language{HIGHLIGHTEN_POSTFIX} sponsored by Mozilla which describes it as a \"safe, concurrent, practical {HIGHLIGHTEN_PREFIX}language{HIGHLIGHTEN_POSTFIX}\", supporting functional and imperative-procedural paradigms. {HIGHLIGHTEN_PREFIX}Rust{HIGHLIGHTEN_POSTFIX} is syntactically similar to C++[according to whom?"));
    }

    #[test]
    fn stemmed_words_snippet_highlight() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage::new(
                &format!(
                    r#"
                        <html>
                            <head>
                                <title>Website for runners</title>
                            </head>
                            <body>
                                {TEST_TEXT}
                            </body>
                        </html>
                    "#
                ),
                "https://www.example.com",
            ))
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let searcher = LocalSearcher::from(index);

        let result = searcher
            .search(&SearchQuery {
                query: "describe".to_string(),
                ..Default::default()
            })
            .expect("Search failed");

        assert_eq!(result.num_hits, 1);
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(snippet_text(result.webpages[0].snippet.clone()), format!("Rust is a systems programming language sponsored by Mozilla which {HIGHLIGHTEN_PREFIX}describes{HIGHLIGHTEN_POSTFIX} it as a \"safe, concurrent, practical language\", supporting functional and imperative-procedural paradigms. Rust is syntactically similar to C++[according to whom?"));
    }

    #[test]
    fn test_stemmed_term() {
        let mut index = Index::temporary().expect("Unable to open index");

        index
            .insert(Webpage::new(
                &format!(
                    r#"
                        <html>
                            <head>
                                <title>Website for runners</title>
                            </head>
                            <body>
                                {TEST_TEXT}
                            </body>
                        </html>
                    "#
                ),
                "https://www.example.com",
            ))
            .expect("failed to insert webpage");
        index.commit().expect("failed to commit index");

        let searcher = LocalSearcher::from(index);

        let result = searcher
            .search(&SearchQuery {
                query: "paradigms".to_string(),
                ..Default::default()
            })
            .expect("Search failed");

        assert_eq!(result.num_hits, 1);
        assert_eq!(result.webpages.len(), 1);
        assert_eq!(
            snippet_text(result.webpages[0].snippet.clone()),
            format!("Rust is a systems programming language sponsored by Mozilla which describes it as a \"safe, concurrent, practical language\", supporting functional and imperative-procedural {HIGHLIGHTEN_PREFIX}paradigms{HIGHLIGHTEN_POSTFIX}. Rust is syntactically similar to C++[according to whom?")
        );
    }

    #[test]
    fn empty_query() {
        assert_eq!(
            snippet_string("this is a test", &[], whatlang::Lang::Eng)
                .fragment
                .as_str(),
            "this is a test"
        );
    }

    #[test]
    fn empty_text() {
        assert_eq!(
            snippet_string("", &["test".to_string()], whatlang::Lang::Eng)
                .fragment
                .as_str(),
            ""
        );

        assert_eq!(
            snippet_string("", &[], whatlang::Lang::Eng)
                .fragment
                .as_str(),
            ""
        );
    }
}
