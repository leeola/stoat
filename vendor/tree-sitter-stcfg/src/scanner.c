#include "tree_sitter/parser.h"

// Binding keys (`Ctrl-=`, `?`, `Alt-Backspace`, ...) are any run of characters
// that is not whitespace, a brace, or `#`, with `-` joining modifier segments.
// Such runs collide with nearly every structural token, so this scanner emits
// a `key_part` only when the run is immediately followed by the binding arrow
// `->`. That lookahead is the same disambiguation the ordered-choice parser in
// config/src/parser.rs gets implicitly, and it cleanly separates a binding from
// a setting, predicate, or function call that starts with the same text.

enum TokenType {
    KEY_PART,
};

void *tree_sitter_stcfg_external_scanner_create(void) { return NULL; }

void tree_sitter_stcfg_external_scanner_destroy(void *payload) { (void)payload; }

unsigned tree_sitter_stcfg_external_scanner_serialize(void *payload, char *buffer) {
    (void)payload;
    (void)buffer;
    return 0;
}

void tree_sitter_stcfg_external_scanner_deserialize(void *payload, const char *buffer,
                                                    unsigned length) {
    (void)payload;
    (void)buffer;
    (void)length;
}

static bool is_whitespace(int32_t c) {
    return c == ' ' || c == '\t' || c == '\r' || c == '\n';
}

static bool is_key_char(int32_t c) {
    return c != 0 && !is_whitespace(c) && c != '{' && c != '}' && c != '#';
}

bool tree_sitter_stcfg_external_scanner_scan(void *payload, TSLexer *lexer,
                                             const bool *valid_symbols) {
    (void)payload;
    if (!valid_symbols[KEY_PART]) {
        return false;
    }

    while (is_whitespace(lexer->lookahead)) {
        lexer->advance(lexer, true);
    }

    bool consumed = false;
    while (is_key_char(lexer->lookahead)) {
        lexer->advance(lexer, false);
        consumed = true;
    }
    if (!consumed) {
        return false;
    }
    lexer->mark_end(lexer);

    while (is_whitespace(lexer->lookahead)) {
        lexer->advance(lexer, true);
    }
    if (lexer->lookahead != '-') {
        return false;
    }
    lexer->advance(lexer, true);
    if (lexer->lookahead != '>') {
        return false;
    }

    lexer->result_symbol = KEY_PART;
    return true;
}
