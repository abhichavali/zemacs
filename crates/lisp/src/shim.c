/* The C half of the ECL bridge.
 *
 * ECL's API is macros all the way down: ECL_NIL, ECL_BASE_STRING_P, cl_object
 * tagging, cl_list's varargs. None of that survives a hand-written Rust
 * `extern "C"` block, so the boundary lives here — Rust only ever sees plain
 * scalars and NUL-terminated UTF-8, in both directions.
 *
 * Most zemacs primitives are a side effect: they convert their Lisp arguments
 * and call back into Rust, which either applies the command to the shared
 * editor or pushes it down a channel to the app. The readers (`%query`) come
 * back the other way, as a string of Lisp source this file hands to READ.
 */

#include <ecl/ecl.h>
#include <stdlib.h>
#include <string.h>

/* Implemented in Rust (src/lib.rs). A NULL string means "absent". */
extern void rs_set_font_size(double size);
extern void rs_set_background(double r, double g, double b);
extern void rs_set_foreground(double r, double g, double b);
extern void rs_set_syntax_color(const char *face, double r, double g, double b);
extern void rs_set_face_style(const char *face, int bold, int italic);
extern void rs_reset_faces(void);
extern void rs_set_line_numbers(int on);
extern void rs_set_tab_width(long n);
extern void rs_set_text_width(long n);
extern void rs_set_modeline_relief(long n);
extern void rs_set_modeline_pad(long n);
extern void rs_message(const char *text);
extern void rs_quit(void);
extern void rs_dashboard_banner(const char *text);
extern void rs_dashboard_logo(long id);
extern void rs_clear_dashboard_items(void);
extern void rs_dashboard_item(const char *key, const char *label,
                              const char *action, const char *hint);
extern void rs_define_key(const char *mode, const char *keys, const char *command);
extern void rs_find_file(const char *path);
extern void rs_save_file(const char *path);
extern void rs_show_dashboard(void);
extern void rs_insert(const char *text);
extern void rs_set_completion_style(const char *style);
extern void rs_clear_commands(void);
extern void rs_register_command(const char *name);
extern void rs_set_line_overflow(const char *mode);
extern void rs_set_relative_line_numbers(int on);
extern void rs_set_major_mode(const char *name);
extern void rs_set_no_gutter_modes(const char *modes);
extern void rs_set_minor_mode(const char *name, int on);
/* Returns Rust-owned memory: hand it to rs_free_string, never to free(3). */
extern char *rs_query(const char *name, long a, long b);
/* The write half of rs_query: one verb, one optional string, two integers. */
extern void rs_do(const char *verb, const char *arg, long a, long b);
extern void rs_free_string(char *p);
extern void rs_goto_char(long n);
extern void rs_delete_region(long a, long b);
extern void rs_replace_region(long a, long b, const char *text);
extern long rs_make_marker(long pos, int advance);
extern void rs_set_marker(long id, long pos);
extern void rs_delete_marker(long id);
extern void rs_set_evil_state(const char *name);
extern void rs_open_prompt(const char *kind);
/* --- overlays and inline images (see the block near the bottom of this file) */
extern long rs_make_overlay(long start, long end);
extern long rs_latex_preview(const char *source);
extern long rs_image_file(const char *path, long ems);
/* Returns Rust-owned memory, like rs_query: rs_free_string, never free(3). */
extern char *rs_highlight(const char *lang, const char *text);
/* --- JSON-RPC subprocesses (see the block near the bottom of this file) --- */
extern long rs_rpc_start(const char *program, const char *args, const char *cwd);
extern long rs_rpc_send(long conn, const char *method, const char *params,
                        int want_reply);
extern void rs_rpc_respond(long conn, const char *id, const char *result,
                           const char *error);
extern void rs_rpc_stop(long conn);
/* Returns Rust-owned memory, like rs_query: rs_free_string, never free(3). */
extern char *rs_json_quote(const char *text);
/* --- end of the JSON-RPC externs ----------------------------------------- */

/* --- The string model ----------------------------------------------------
 *
 * **Lisp holds text as characters**, on both sides of this file. `λ` is one
 * Lisp character whether it arrived in a `load'ed file, in a form Rust queued
 * for evaluation, or in the answer to `(buffer-string)'; UTF-8 exists only in
 * the `char *' between the two languages, and it is encoded and decoded here.
 *
 * It was bytes until recently — every reader answered a base string of UTF-8
 * bytes, `%byte-index' and `%char-index' converted between the two counts, and
 * `utf8-text' in `modes.lisp' decoded at each of the dozen places text left a
 * buffer. The failure that model has is not that it is inconvenient: it is that
 * ECL's reader parses a byte string as Latin-1, so `(message "λ")' arriving
 * from a keybinding reached the status line as `Î»' while the same form in a
 * file was fine. The two halves — decoding what arrives, and stopping
 * everything above from decoding it a second time — had to move together,
 * because either alone makes buffer text and eval'd source incomparable and
 * `(search-forward "γ" 0)' answers NIL.
 *
 * So: `utf8_string' below is the only decoder, `dup_utf8' the only encoder, and
 * nothing in `runtime/' knows that UTF-8 exists. */

/* --- C -> Lisp ------------------------------------------------------------ */

/* Bytes in the sequence B opens. A stray continuation byte (0x80..0xBF) opens a
 * one-byte sequence and so passes through as the character of that code rather
 * than being dropped: what reads this is an editor, and text it cannot make
 * sense of should still be visible, because the alternative is a screen that
 * silently disagrees with the file behind it. */
static cl_index utf8_run(unsigned char b) {
  if (b < 0xC0)
    return 1;
  if (b < 0xE0)
    return 2;
  if (b < 0xF0)
    return 3;
  return 4;
}

/* A UTF-8 C string as a Lisp character string.
 *
 * Two passes, because a simple string has no fill pointer and the character
 * count is not the byte count. Both passes step with `utf8_run' and clamp a
 * truncated tail the same way, so they cannot disagree about how many
 * characters there are — which they would if the count were "bytes that are not
 * continuations" and the decoder emitted one character for a stray one.
 *
 * Pure ASCII comes back as a *base* string, which for ASCII is the same text in
 * a quarter of the memory: a base character's code is its byte. That is the
 * common case — a query answer is nearly always source with no accent in it —
 * and it is what keeps `buffer-string' on a large file as cheap as it was. */
static cl_object utf8_string(const char *s) {
  const unsigned char *p = (const unsigned char *)s;
  cl_index n = (cl_index)strlen(s);

  int ascii = 1;
  cl_index nchars = 0;
  for (cl_index i = 0; i < n;) {
    if (p[i] >= 0x80)
      ascii = 0;
    cl_index len = utf8_run(p[i]);
    i += (i + len > n) ? 1 : len;
    nchars++;
  }
  if (ascii)
    return ecl_make_simple_base_string((char *)s, (cl_fixnum)n);

  cl_object out = ecl_alloc_simple_extended_string(nchars);
  cl_index k = 0;
  for (cl_index i = 0; i < n;) {
    unsigned char b = p[i];
    cl_index len = utf8_run(b);
    unsigned int c;
    if (len == 1 || i + len > n) {
      c = b; /* ASCII, a stray continuation byte, or a truncated tail */
      len = 1;
    } else {
      c = b & (0xFFu >> (len + 1));
      for (cl_index j = 1; j < len; j++)
        c = (c << 6) | (p[i + j] & 0x3F);
    }
    ecl_char_set(out, k++, (ecl_character)c);
    i += len;
  }
  return out;
}

/* `ecl_read_from_cstring' with the string decoded first. Every answer Rust
 * gives is Lisp *source*, and ECL's own macro makes a base string of it — which
 * is where the whole Latin-1 mis-read used to enter. The boot forms below still
 * use the macro: they are ASCII C literals, and reading them is not a
 * boundary. */
static cl_object read_utf8(const char *src) {
  return si_string_to_object(1, utf8_string(src));
}

/* --- Lisp -> C ------------------------------------------------------------ */

/* Owned UTF-8 copy of any object's PRINC form; NULL for NIL. Going through
 * PRINC rather than demanding a string means #\f, "f", 'find-file and #P"/x"
 * all work, so callers never have to think about types.
 *
 * The fast path is *ASCII* base strings, not base strings: a base string may
 * hold characters up to 255 — `(code-char 233)' is one — and those are Latin-1
 * characters that have to be encoded like any other. Copying its bytes would
 * hand Rust something that is not UTF-8 at all. */
static char *dup_utf8(cl_object x) {
  if (x == ECL_NIL)
    return NULL;
  cl_object s = cl_princ_to_string(x);

  if (ECL_BASE_STRING_P(s)) {
    cl_index n = s->base_string.fillp;
    ecl_base_char *self = s->base_string.self;
    cl_index i = 0;
    while (i < n && self[i] < 0x80)
      i++;
    if (i == n) {
      char *buf = (char *)malloc((size_t)n + 1);
      if (!buf)
        return NULL;
      memcpy(buf, self, (size_t)n);
      buf[n] = '\0';
      return buf;
    }
  }

  cl_fixnum n = ecl_length(s);
  char *buf = (char *)malloc((size_t)n * 4 + 1);
  if (!buf)
    return NULL;
  size_t k = 0;
  for (cl_fixnum i = 0; i < n; i++) {
    unsigned int c = (unsigned int)ecl_char(s, (cl_index)i);
    if (c < 0x80) {
      buf[k++] = (char)c;
    } else if (c < 0x800) {
      buf[k++] = (char)(0xC0 | (c >> 6));
      buf[k++] = (char)(0x80 | (c & 0x3F));
    } else if (c < 0x10000) {
      buf[k++] = (char)(0xE0 | (c >> 12));
      buf[k++] = (char)(0x80 | ((c >> 6) & 0x3F));
      buf[k++] = (char)(0x80 | (c & 0x3F));
    } else {
      buf[k++] = (char)(0xF0 | (c >> 18));
      buf[k++] = (char)(0x80 | ((c >> 12) & 0x3F));
      buf[k++] = (char)(0x80 | ((c >> 6) & 0x3F));
      buf[k++] = (char)(0x80 | (c & 0x3F));
    }
  }
  buf[k] = '\0';
  return buf;
}

/* dup_utf8 but never NULL — for arguments that are always strings. */
static char *dup_utf8_or_empty(cl_object x) {
  char *s = dup_utf8(x);
  return s ? s : strdup("");
}

/* The same, but NIL means the empty string rather than the three characters
 * "NIL". For optional arguments, where a caller that has nothing to say passes
 * NIL and PRINC would otherwise spell the absence out and hand it over as
 * content — a dashboard row whose keybinding hint reads "NIL" is what that
 * looks like from the outside. */
static char *dup_utf8_or_empty_nil(cl_object x) {
  return x == ECL_NIL ? strdup("") : dup_utf8_or_empty(x);
}

/* --- Primitives --------------------------------------------------------- */
/* Numbers are converted before strings everywhere: a type error in ecl_to_*
 * unwinds non-locally, and doing it first means there is no malloc'd block
 * live at that point. */

static cl_object f_set_font_size(cl_object n) {
  rs_set_font_size(ecl_to_double(n));
  return ECL_NIL;
}

static cl_object f_set_background(cl_object r, cl_object g, cl_object b) {
  rs_set_background(ecl_to_double(r), ecl_to_double(g), ecl_to_double(b));
  return ECL_NIL;
}

static cl_object f_set_foreground(cl_object r, cl_object g, cl_object b) {
  rs_set_foreground(ecl_to_double(r), ecl_to_double(g), ecl_to_double(b));
  return ECL_NIL;
}

static cl_object f_set_syntax_color(cl_object face, cl_object r, cl_object g,
                                    cl_object b) {
  double rr = ecl_to_double(r), gg = ecl_to_double(g), bb = ecl_to_double(b);
  char *f = dup_utf8_or_empty(face);
  rs_set_syntax_color(f, rr, gg, bb);
  free(f);
  return ECL_NIL;
}

/* Generalised booleans, so `(set-face-style "keyword" t nil)' and
 * `(set-face-style "keyword" 'yes nil)' agree — the same courtesy the boolean
 * settings extend, and the reason `set-face' upstairs can pass a &key straight
 * through without coercing it. No number to convert first, so the string is
 * duplicated at the top without the usual ordering worry. */
static cl_object f_set_face_style(cl_object face, cl_object bold,
                                  cl_object italic) {
  char *f = dup_utf8_or_empty(face);
  rs_set_face_style(f, bold != ECL_NIL, italic != ECL_NIL);
  free(f);
  return ECL_NIL;
}

/* Empties the face table. Only `load-theme' has any business calling it: on
 * its own it is not "go back to the defaults" but "have no colours at all",
 * and what makes that safe is that the next form after it is a theme. */
static cl_object f_reset_faces(void) {
  rs_reset_faces();
  return ECL_NIL;
}

static cl_object f_set_line_numbers(cl_object on) {
  rs_set_line_numbers(on != ECL_NIL);
  return ECL_NIL;
}

static cl_object f_set_tab_width(cl_object n) {
  rs_set_tab_width((long)ecl_to_fixnum(n));
  return ECL_NIL;
}

/* NIL is 0 is "off", so `(set-text-width nil)' reads as turning it off rather
 * than signalling — the same courtesy the boolean settings extend. */
static cl_object f_set_text_width(cl_object n) {
  rs_set_text_width(n == ECL_NIL ? 0 : (long)ecl_to_fixnum(n));
  return ECL_NIL;
}

/* No abs/max here: a negative relief is a *sunken* modeline, not an error. */
static cl_object f_set_modeline_relief(cl_object n) {
  rs_set_modeline_relief((long)ecl_to_fixnum(n));
  return ECL_NIL;
}

static cl_object f_set_modeline_pad(cl_object n) {
  rs_set_modeline_pad((long)ecl_to_fixnum(n));
  return ECL_NIL;
}

static cl_object f_message(cl_object text) {
  char *s = dup_utf8_or_empty(text);
  rs_message(s);
  free(s);
  return ECL_NIL;
}

static cl_object f_quit(void) {
  rs_quit();
  return ECL_NIL;
}

static cl_object f_dashboard_banner(cl_object text) {
  char *s = dup_utf8_or_empty(text);
  rs_dashboard_banner(s);
  free(s);
  return ECL_NIL;
}

/* Takes the id `image-file' answered, so the dashboard reuses the one image
 * pipeline rather than learning to read files. NIL means "no logo". */
static cl_object f_dashboard_logo(cl_object id) {
  rs_dashboard_logo(id == ECL_NIL ? 0 : ecl_to_fixnum(id));
  return ECL_NIL;
}

static cl_object f_clear_dashboard_items(void) {
  rs_clear_dashboard_items();
  return ECL_NIL;
}

static cl_object f_dashboard_item(cl_object key, cl_object label,
                                  cl_object action, cl_object hint) {
  char *k = dup_utf8_or_empty(key);
  char *l = dup_utf8_or_empty(label);
  char *a = dup_utf8_or_empty(action);
  char *h = dup_utf8_or_empty_nil(hint);
  rs_dashboard_item(k, l, a, h);
  free(k);
  free(l);
  free(a);
  free(h);
  return ECL_NIL;
}

static cl_object f_define_key(cl_object mode, cl_object keys, cl_object cmd) {
  char *m = dup_utf8_or_empty(mode);
  char *k = dup_utf8_or_empty(keys);
  char *c = dup_utf8_or_empty(cmd);
  rs_define_key(m, k, c);
  free(m);
  free(k);
  free(c);
  return ECL_NIL;
}

static cl_object f_find_file(cl_object path) {
  char *p = dup_utf8_or_empty(path);
  rs_find_file(p);
  free(p);
  return ECL_NIL;
}

/* NIL path => save in place. The Lisp wrapper `save-file` supplies the
 * &optional so callers can write plain `(save-file)`. */
static cl_object f_save_file(cl_object path) {
  char *p = dup_utf8(path);
  rs_save_file(p);
  free(p);
  return ECL_NIL;
}

static cl_object f_set_line_overflow(cl_object mode) {
  char *m = dup_utf8_or_empty(mode);
  rs_set_line_overflow(m);
  free(m);
  return ECL_NIL;
}

static cl_object f_set_relative_line_numbers(cl_object on) {
  rs_set_relative_line_numbers(on != ECL_NIL);
  return ECL_NIL;
}

static cl_object f_set_major_mode(cl_object name) {
  char *n = dup_utf8_or_empty(name);
  rs_set_major_mode(n);
  free(n);
  return ECL_NIL;
}

/* A list of mode names, or one space-separated string. The list spelling is
 * what a config wants to write; the string is what the envelope carries.
 *
 * The 3 is load-bearing. ECL's NARG counts *every* argument, the destination
 * and the control string included — `ecl_va_start(a, p, n, 2)' subtracts the
 * two named ones — so this is (format nil "~{~a~^ ~}" modes) and nothing more.
 * A 4 here says there is a second format argument, and ECL's CL_GRAB_REST_ARGS
 * duly reads one out of the outgoing-argument area and conses whatever word is
 * sitting there into a list as a cl_object. It is usually harmless and it is
 * not reliably so: with that slot poisoned it is an immediate
 * SEGMENTATION-VIOLATION inside FORMAT, and even when FORMAT never looks at it
 * the garbage is a live root the collector will try to trace. */
static cl_object f_set_no_gutter_modes(cl_object modes) {
  cl_object s = modes;
  if (ECL_LISTP(modes) && modes != ECL_NIL) {
    s = cl_format(3, ECL_NIL, ecl_make_simple_base_string("~{~a~^ ~}", -1),
                  modes);
  } else if (modes == ECL_NIL) {
    s = ecl_make_simple_base_string("", 0);
  }
  char *m = dup_utf8_or_empty(s);
  rs_set_no_gutter_modes(m);
  free(m);
  return ECL_NIL;
}

/* NIL turns the mode off; anything else turns it on. */
static cl_object f_set_minor_mode(cl_object name, cl_object on) {
  int enable = (on != ECL_NIL);
  char *n = dup_utf8_or_empty(name);
  rs_set_minor_mode(n, enable);
  free(n);
  return ECL_NIL;
}

static cl_object f_show_dashboard(void) {
  rs_show_dashboard();
  return ECL_NIL;
}

static cl_object f_insert(cl_object text) {
  char *s = dup_utf8_or_empty(text);
  rs_insert(s);
  free(s);
  return ECL_NIL;
}

static cl_object f_set_completion_style(cl_object style) {
  char *s = dup_utf8_or_empty(style);
  rs_set_completion_style(s);
  free(s);
  return ECL_NIL;
}

static cl_object f_clear_commands(void) {
  rs_clear_commands();
  return ECL_NIL;
}

/* PRINCing means `(register-command 'text-scale-increase)` also works, but it
 * would arrive upcased — refresh-commands downcases before calling. */
static cl_object f_register_command(cl_object name) {
  char *s = dup_utf8_or_empty(name);
  rs_register_command(s);
  free(s);
  return ECL_NIL;
}

/* --- Reading and writing the buffer ------------------------------------- */

/* The one reader. Rust answers with Lisp source and this READs it, so a single
 * C signature serves integers, strings, NIL, conses and lists alike — see the
 * zemacs_core::query docs.
 *
 * `read_utf8' and not ECL's `ecl_read_from_cstring': the answer is UTF-8 and
 * ECL's macro would read it as Latin-1. This is the single place `(buffer-string)`
 * becomes characters, which is why `(length (buffer-string))` is the editor's own
 * `point-max' on any buffer at all. */
static cl_object f_query(cl_object name, cl_object a, cl_object b) {
  long ia = (a == ECL_NIL) ? 0 : (long)ecl_to_fixnum(a);
  long ib = (b == ECL_NIL) ? 0 : (long)ecl_to_fixnum(b);
  char *n = dup_utf8_or_empty(name);
  char *src = rs_query(n, ia, ib);
  free(n);
  if (!src)
    return ECL_NIL;
  cl_object form = read_utf8(src);
  rs_free_string(src);
  return form;
}

/* The one writer, mirroring f_query. Every command whose arguments fit "a verb,
 * maybe a string, up to two integers" goes through here, so adding one is an arm
 * of the match in zemacs_lisp::command_for and a one-line DEFUN below — nothing
 * in C. NIL for either integer means 0, as in f_query. */
static cl_object f_do(cl_object verb, cl_object arg, cl_object a, cl_object b) {
  long ia = (a == ECL_NIL) ? 0 : (long)ecl_to_fixnum(a);
  long ib = (b == ECL_NIL) ? 0 : (long)ecl_to_fixnum(b);
  char *v = dup_utf8_or_empty(verb);
  char *s = dup_utf8(arg);
  rs_do(v, s, ia, ib);
  free(v);
  free(s);
  return ECL_NIL;
}

static cl_object f_goto_char(cl_object n) {
  rs_goto_char((long)ecl_to_fixnum(n));
  return ECL_NIL;
}

static cl_object f_delete_region(cl_object a, cl_object b) {
  rs_delete_region((long)ecl_to_fixnum(a), (long)ecl_to_fixnum(b));
  return ECL_NIL;
}

static cl_object f_replace_region(cl_object a, cl_object b, cl_object text) {
  long ia = (long)ecl_to_fixnum(a), ib = (long)ecl_to_fixnum(b);
  char *s = dup_utf8_or_empty(text);
  rs_replace_region(ia, ib, s);
  free(s);
  return ECL_NIL;
}

/* The handle is a fixnum, so a marker is a value the reader and PRINT already
 * agree on — no new Lisp type, and `marker-position` can come back through
 * %QUERY like every other reader. NIL insertion type is Emacs' default: the
 * marker stays put when text is inserted exactly at it. */
static cl_object f_make_marker(cl_object pos, cl_object advance) {
  long p = (pos == ECL_NIL) ? 0 : (long)ecl_to_fixnum(pos);
  return ecl_make_fixnum(rs_make_marker(p, advance != ECL_NIL));
}

static cl_object f_set_marker(cl_object m, cl_object pos) {
  rs_set_marker((long)ecl_to_fixnum(m), (long)ecl_to_fixnum(pos));
  return ECL_NIL;
}

static cl_object f_delete_marker(cl_object m) {
  rs_delete_marker((long)ecl_to_fixnum(m));
  return ECL_NIL;
}

/* --- overlays and inline images ------------------------------------------ */
/* Two primitives, for the only two things here that have to answer with a
 * value. Everything that *changes* an overlay fits "a verb, maybe a string, two
 * integers" and goes through %DO — see `command_for' on the Rust side.
 *
 * Like a marker, an overlay handle is a fixnum, so it needs no new Lisp type and
 * `overlay-position' can come back through %QUERY like every other reader. NIL
 * for an empty range, so `(when (make-overlay a b) ...)' is honest. */
static cl_object f_make_overlay(cl_object start, cl_object end) {
  long a = (long)ecl_to_fixnum(start), b = (long)ecl_to_fixnum(end);
  long id = rs_make_overlay(a, b);
  return id ? ecl_make_fixnum(id) : ECL_NIL;
}

/* Render one LaTeX fragment; the answer is the id an `image' overlay property
 * takes, or NIL with the reason already in the status line.
 *
 * This is the slow one — a cold render shells out to latex and dvipng — and it
 * is deliberately synchronous: the Lisp thread waits, the editor does not. That
 * is the whole point of the image having a thread of its own. */
static cl_object f_latex_preview(cl_object source) {
  char *s = dup_utf8_or_empty(source);
  long id = rs_latex_preview(s);
  free(s);
  return id ? ecl_make_fixnum(id) : ECL_NIL;
}

/* Read an image file; the answer is the id an `image' overlay property takes,
 * or NIL with the reason already in the status line.
 *
 * The sibling of LATEX-PREVIEW above and it earns its C entry point for the same
 * reason: it has to hand a value back. WIDTH is the widest the figure may be
 * drawn, as a hundredth of an em — an integer, because the whole overlay bridge
 * is integers and strings, and resolved against the *device* em on the Rust
 * side because that is the one thing this image cannot know. NIL or 0 means "as
 * authored". */
static cl_object f_image_file(cl_object path, cl_object width) {
  char *p = dup_utf8_or_empty(path);
  long ems = (width == ECL_NIL) ? 0 : (long)ecl_to_fixnum(width);
  long id = rs_image_file(p, ems);
  free(p);
  return id ? ecl_make_fixnum(id) : ECL_NIL;
}

/* Highlight a string as a language: `((START END "face") ...)` in char offsets.
 *
 * The third primitive that has to answer with a value, and the first that is a
 * *pure function* — it never touches the editor, so no lock is taken and the
 * text it parses is whatever the caller hands over rather than whatever happens
 * to be in the live buffer. That is what lets org colour a `#+begin_src python`
 * block: one language per buffer is a fact about the highlighting thread, and
 * this is the way around it.
 *
 * Comes back through READ exactly as f_query does, for the same reason — one C
 * signature, and the answer is Lisp source. */
static cl_object f_highlight(cl_object lang, cl_object text) {
  char *l = dup_utf8_or_empty(lang);
  char *t = dup_utf8_or_empty(text);
  char *src = rs_highlight(l, t);
  free(l);
  free(t);
  if (!src)
    return ECL_NIL;
  cl_object form = read_utf8(src);
  rs_free_string(src);
  return form;
}

/* The editor's own pickers: "file" "buffer" "command" "line" "grep"
 * "project-file" "ex" "search". What the answer does is fixed by the kind, so
 * this puts the picker up and does not hand Lisp the reply. */
static cl_object f_open_prompt(cl_object kind) {
  char *k = dup_utf8_or_empty(kind);
  rs_open_prompt(k);
  free(k);
  return ECL_NIL;
}

static cl_object f_set_evil_state(cl_object name) {
  char *n = dup_utf8_or_empty(name);
  rs_set_evil_state(n);
  free(n);
  return ECL_NIL;
}

/* --- JSON-RPC subprocesses ----------------------------------------------- */
/* Five primitives and no protocol. Three of them hand a value back — a
 * connection handle, a request id, an escaped string — which is what earns them
 * a C entry point rather than a `%DO` verb; the other two carry more strings
 * than `%DO`'s single one.
 *
 * A connection handle is a fixnum for the same reason a marker handle is: no
 * new Lisp type, and PRINT and READ already agree about what one looks like. 0
 * is never a live connection, so the wrappers in `runtime/rpc.lisp' turn it
 * into NIL and every caller's `(when conn ...)' reads correctly.
 *
 * Nothing here delivers a reply. Replies arrive on the main thread and are
 * queued for this one as `(%rpc-event CONN KIND FORM)'; see the block in
 * lib.rs. */

static cl_object f_rpc_start(cl_object program, cl_object args, cl_object cwd) {
  char *p = dup_utf8_or_empty(program);
  char *a = dup_utf8_or_empty(args);
  char *c = dup_utf8(cwd);
  long conn = rs_rpc_start(p, a, c);
  free(p);
  free(a);
  free(c);
  return ecl_make_fixnum(conn);
}

/* NIL params means the message carries none at all, which is not the same as
 * carrying `null' — a server checking arity rejects the latter. */
static cl_object f_rpc_send(cl_object conn, cl_object method, cl_object params,
                            cl_object want_reply) {
  long c = (long)ecl_to_fixnum(conn);
  int reply = (want_reply != ECL_NIL);
  char *m = dup_utf8_or_empty(method);
  char *p = dup_utf8(params);
  long id = rs_rpc_send(c, m, p, reply);
  free(m);
  free(p);
  return ecl_make_fixnum(id);
}

static cl_object f_rpc_respond(cl_object conn, cl_object id, cl_object result,
                               cl_object error) {
  long c = (long)ecl_to_fixnum(conn);
  char *i = dup_utf8_or_empty(id);
  char *r = dup_utf8(result);
  char *e = dup_utf8(error);
  rs_rpc_respond(c, i, r, e);
  free(i);
  free(r);
  free(e);
  return ECL_NIL;
}

static cl_object f_rpc_stop(cl_object conn) {
  rs_rpc_stop((long)ecl_to_fixnum(conn));
  return ECL_NIL;
}

/* The escaping half of the JSON encoder in `runtime/rpc.lisp'. Here rather than
 * there because the string it escapes most often is a whole buffer on its way
 * into a `textDocument/didChange'.
 *
 * serde leaves non-ASCII as itself rather than spelling it `\uXXXX', so the
 * answer is UTF-8 and comes back through `utf8_string' like everything else. It
 * used to come back as a base string and `json-string' in `runtime/rpc.lisp'
 * had to undo that with `utf8-text' — a decode written in Lisp, in the file
 * that could least afford to know about encodings. */
static cl_object f_json_quote(cl_object text) {
  char *s = dup_utf8_or_empty(text);
  char *json = rs_json_quote(s);
  free(s);
  if (!json)
    return ecl_make_simple_base_string("\"\"", -1);
  cl_object out = utf8_string(json);
  rs_free_string(json);
  return out;
}

/* --- end of the JSON-RPC primitives --------------------------------------- */

/* --- Boot --------------------------------------------------------------- */

/* Creating the package has to happen before ecl_def_c_function, which interns
 * into it. Exporting the names here is what makes `zemacs:message` read. */
static const char *PACKAGE_FORM =
    "(defpackage \"ZEMACS\" (:use \"CL\")"
    " (:export \"SET-FONT-SIZE\" \"SET-BACKGROUND\" \"SET-FOREGROUND\""
    "          \"SET-SYNTAX-COLOR\" \"SET-FACE-STYLE\""
    "          \"SET-LINE-NUMBERS\" \"SET-TAB-WIDTH\""
    "          \"SET-TEXT-WIDTH\""
    "          \"SET-MODELINE-RELIEF\" \"SET-MODELINE-PAD\""
    "          \"MESSAGE\" \"QUIT\" \"DASHBOARD-BANNER\""
    "          \"CLEAR-DASHBOARD-ITEMS\" \"%DASHBOARD-ITEM\" \"DEFINE-KEY\""
    "          \"FIND-FILE\" \"SAVE-FILE\" \"SHOW-DASHBOARD\" \"INSERT\""
    "          \"SET-COMPLETION-STYLE\" \"CLEAR-COMMANDS\""
    "          \"REGISTER-COMMAND\" \"SET-LINE-OVERFLOW\""
    "          \"SET-RELATIVE-LINE-NUMBERS\" \"SET-MAJOR-MODE\""
    "          \"SET-NO-GUTTER-MODES\""
    "          \"SET-MINOR-MODE\" \"GOTO-CHAR\" \"DELETE-REGION\""
    "          \"REPLACE-REGION\" \"SET-EVIL-STATE\" \"BUFFER-SUBSTRING\""
    "          \"REGION-TEXT\" \"REGION-BEGINNING\" \"REGION-END\""
    "          \"MAKE-MARKER\" \"POINT-MARKER\" \"MARKER-POSITION\""
    "          \"SET-MARKER\" \"DELETE-MARKER\" \"GOTO-MARKER\""
    "          \"OPEN-PROMPT\"))";

/* Where zemacs keeps things, in Lisp. The counterpart of `config_dir` in
 * `crates/app/src/main.rs`, and it has to agree with it: the auto-saves and
 * backups the editor writes land beside the scratch file, the REPL transcript
 * and the tutorial's progress, all of which are named through this.
 *
 * Booted here rather than defined in `init.lisp` because the modules that use it
 * do not depend on init.lisp in a test — `runtime/lsp.lisp` and half of
 * `runtime/modes/` are loaded on their own by the suite, and a path helper that
 * exists only when a config happens to have been read first is one those files
 * cannot safely call. Where the editor puts its state is a fact about the
 * editor.
 *
 * ponytail: the directory name is written here *and* in `config_dir`, in two
 * languages, and nothing checks that they match. A `(config-dir)` primitive
 * reading the Rust side would; it is one FFI function and a query arm, and it is
 * worth doing the moment a third thing needs to know the path. */
static const char *PATHS_FORM =
    "(progn"
    " (defun zemacs::zemacs-file (name)"
    "   \"NAME inside ~/.zemacs.d/.\""
    "   (merge-pathnames (concatenate 'string \".zemacs.d/\" name)"
    "                    (user-homedir-pathname)))"
    /* Where the *shipped* Lisp lives, as against where your config lives. The
     * two were the same directory for as long as the config was the copy in the
     * source tree; they are not once `init.lisp' is yours and sits in
     * `~/.zemacs.d/', which has no themes/ or modes/ beside it. Booted from the
     * environment for `zemacs-file's reason — a module loaded on its own by the
     * test suite cannot depend on a config having been read first — and out of
     * `$ZEMACS_RUNTIME' rather than a compiled-in path so that one binary can
     * be pointed at a checkout, an install prefix or a bundle.
     *
     * NIL when unset, which is every test that spawns the image without the
     * application around it. `init.lisp' falls back to its own directory there,
     * and that is exactly right: the file being loaded by absolute path out of
     * `runtime/' *is* next to the runtime. */
    " (defparameter zemacs::*runtime-dir*"
    "   (let ((d (ext:getenv \"ZEMACS_RUNTIME\")))"
    "     (when (and d (plusp (length d)))"
    "       (pathname (concatenate 'string (string-right-trim \"/\" d) \"/\"))))"
    "   \"Directory of the shipped Lisp library, or NIL if the editor did not"
    " say. See `runtime-file' in library.lisp.\"))";

/* The readers. Each takes no arguments and forwards to %QUERY, so adding one is
 * a name here and an arm of the match in zemacs_core::query — nothing in C.
 * Interned and exported at runtime rather than listed in the DEFPACKAGE above,
 * so the two lists cannot drift apart. */
static const char *QUERIES_FORM =
    "(progn"
    /* Bound as well as iterated, so `init.lisp' can keep the whole set out of
     * the M-x list without hand-maintaining a copy of it. A reader is a noun
     * that answers a question, not a command worth offering. */
    " (defparameter zemacs::*readers*"
    "   '(\"point\" \"point-min\" \"point-max\" \"line-number\""
    "              \"column\" \"line-count\""
    "              \"buffer-string\" \"buffer-size\""
    "              \"buffer-name\" \"buffer-file-name\" \"buffer-modified-p\""
    "              \"buffer-read-only-p\" \"buffer-list\" \"buffer-info\""
    "              \"register\"" /* clipboard */
    "              \"major-mode\""
    "              \"minor-modes\" \"evil-state\" \"evil-keymaps\""
    "              \"region\" \"region-ranges\""
    "              \"window-scroll\" \"window-height\" \"frame-count\""
    "              \"window-list\" \"window-id\" \"frame-index\""
    "              \"key-bindings\" \"command-list\" \"status\" \"face-list\""
    "              \"font-size\" \"tab-width\" \"text-width\" \"line-numbers-p\""
    "              \"relative-line-numbers-p\" \"line-overflow\""
    "              \"scroll-past-end-p\""
    "              \"completion-style\" \"modeline-relief\" \"modeline-pad\""
    "     \"background\" \"foreground\"))"
    /* LET* so each closure captures its own binding: DOLIST is allowed to
     * reuse one for the whole loop, which would leave every reader asking the
     * last question. */
    " (dolist (q zemacs::*readers*)"
    "   (let* ((name q) (sym (intern (string-upcase name) \"ZEMACS\")))"
    "     (setf (symbol-function sym) (lambda () (zemacs::%query name 0 0)))"
    "     (export sym \"ZEMACS\")))"
    " (defun zemacs::buffer-substring (a b) (zemacs::%query \"buffer-substring\" a b))"
    /* The readers that take an argument, hand-written because the loop above
     * only makes zero-argument ones. A line number is 1-based, as `line-number'
     * reports one, and NIL means the line point is on — so the zero-argument
     * spelling these three have always had still means what it did. */
    " (defun zemacs::line-start (&optional line) (zemacs::%query \"line-start\" (or line 0) 0))"
    " (defun zemacs::line-end (&optional line) (zemacs::%query \"line-end\" (or line 0) 0))"
    " (defun zemacs::line-string (&optional line) (zemacs::%query \"line-string\" (or line 0) 0))"
    /* The other end of the bracket at POS, or just before it — which is where
     * point sits the instant you finish typing a closing one, and is the whole
     * reason this answers for two positions rather than one. NIL when there is
     * no bracket there or it is unbalanced. `runtime/modes/show-paren.lisp' is
     * the only caller; `%' in the evil grammar asks the same primitive, so the
     * highlight and the jump can never disagree. */
    " (defun zemacs::matching-bracket (&optional pos)"
    "   (zemacs::%query \"matching-bracket\" (or pos (zemacs::point)) 0))"
    " (pushnew \"matching-bracket\" zemacs::*readers* :test #'string=)"
    /* The last N messages, or all of them. This is `*Messages*'. */
    " (defun zemacs::messages (&optional n) (zemacs::%query \"messages\" (or n 0) 0))"
    /* A query takes integers, and a face is named by a string everywhere else in
     * the API — so the name is turned into its position in `face-list' here
     * rather than the vocabulary being written down twice. */
    " (defun zemacs::syntax-color (face)"
    "   (let ((i (position (string-downcase (string face)) (zemacs::face-list)"
    "                      :test #'string=)))"
    "     (when i (zemacs::%query \"syntax-color\" i 0))))"
    /* The three region accessors Emacs code reaches for constantly. NIL with
     * nothing selected, so `(when (region-text) ...)` is the idiom. */
    " (defun zemacs::region-beginning () (car (zemacs::region)))"
    " (defun zemacs::region-end () (cdr (zemacs::region)))"
    " (defun zemacs::region-text ()"
    "   (let ((r (zemacs::region)))"
    "     (when r (zemacs::buffer-substring (car r) (cdr r)))))"
    /* Markers. A handle answers NIL once it is deleted or once its buffer is
     * not the live one, so `(when (marker-position m) ...)` is the idiom, the
     * same shape as `region`.
     *
     * ponytail: `(make-marker)` puts one at point rather than nowhere, so there
     * is no positionless state to carry — the Emacs idiom `(make-marker)` then
     * `(set-marker m p)` is `(make-marker p)` here. Likewise the insertion type
     * is fixed at creation: `set-marker-insertion-type` would be a third
     * primitive for a flag nothing has needed to change yet. */
    " (defun zemacs::marker-position (m) (zemacs::%query \"marker-position\" m 0))"
    " (defun zemacs::make-marker (&optional pos advance)"
    "   (zemacs::%make-marker (or pos (zemacs::point)) advance))"
    " (defun zemacs::point-marker () (zemacs::make-marker))"
    /* Two locks rather than one, so a keystroke can land between reading the
     * marker and moving point. Harmless: the marker is adjusted by whatever
     * that keystroke did, so the worst case is arriving where it now is. */
    " (defun zemacs::goto-marker (m)"
    "   (let ((p (zemacs::marker-position m)))"
    "     (when p (zemacs::goto-char p) p))))";

/* Written with explicit `zemacs::` prefixes because a single EVAL reads the
 * whole PROGN before IN-PACKAGE could take effect. */
static const char *HELPERS_FORM =
    "(progn"
    " (defun zemacs::save-file (&optional path) (zemacs::%save-file path))"
    /* *package* must be bound around the READ, not just the EVAL: LOAD only
     * binds it for the duration of the load, so by the time a keybinding fires
     * we are back in CL-USER and `(find-file)` would read as an undefined
     * CL-USER::FIND-FILE. Binding it to ZEMACS is what makes both the host
     * primitives and the user's own DEFUNs in init.lisp resolve. */
    /* Reports the value of the *last* form, the way `eval-last-sexp' echoes
     * into the echo area — but stays silent when that value is NIL. Every
     * command that already called `message' returns NIL, and echoing it would
     * wipe out the message the command just produced. */
    " (defun zemacs::eval-string (s)"
    "   (handler-case"
    "       (let ((*package* (find-package \"ZEMACS\")))"
    "         (with-input-from-string (in s)"
    "           (let ((value nil))"
    "             (loop for form = (read in nil 'zemacs::%eof)"
    "                   until (eq form 'zemacs::%eof)"
    "                   do (setf value (eval form)))"
    "             (when value"
    "               (zemacs::message"
    /* ~S so \"3\" and 3 are distinguishable; the print limits keep a huge or
     * circular structure from becoming a status line nobody can read. */
    "                (let ((*print-length* 32) (*print-level* 4)"
    "                      (*print-circle* t))"
    "                  (format nil \"~s\" value)))))))"
    "     (error (e) (zemacs::message (format nil \"lisp error: ~a\" e)))))"
    " (defun zemacs::load-init (path)"
    "   (handler-case (load path :verbose nil :print nil)"
    "     (error (e)"
    "       (zemacs::message (format nil \"init.lisp error: ~a\" e))))))";

/* The rest of the language, written in Lisp because it is composition.
 *
 * Nothing below needs anything from C that %DO and %QUERY do not already give
 * it: a command wrapper is a name and an argument order, and a text helper is
 * arithmetic on offsets. Writing these here rather than as primitives is what
 * keeps the C surface at the size of the *envelope* (a verb, a string, two
 * integers) instead of the size of the API.
 *
 * `zemacs::' on every name for the same reason HELPERS_FORM has it: one EVAL
 * reads the whole PROGN before an IN-PACKAGE could take effect. */
static const char *LIBRARY_FORM =
    "(progn"
    /* --- commands ------------------------------------------------------- */
    " (defun zemacs::undo () (zemacs::%do \"undo\" nil 0 0))"
    " (defun zemacs::redo () (zemacs::%do \"redo\" nil 0 0))"
    " (defun zemacs::split-window-right () (zemacs::%do \"split-window-right\" nil 0 0))"
    " (defun zemacs::split-window-below () (zemacs::%do \"split-window-below\" nil 0 0))"
    " (defun zemacs::delete-window () (zemacs::%do \"delete-window\" nil 0 0))"
    " (defun zemacs::other-window () (zemacs::%do \"other-window\" nil 0 0))"
    " (defun zemacs::select-window (id) (zemacs::%do \"select-window\" nil id 0))"
    " (defun zemacs::new-frame () (zemacs::%do \"new-frame\" nil 0 0))"
    " (defun zemacs::delete-frame () (zemacs::%do \"delete-frame\" nil 0 0))"
    " (defun zemacs::select-frame (i) (zemacs::%do \"select-frame\" nil i 0))"
    " (defun zemacs::scroll-lines (n) (zemacs::%do \"scroll\" nil n 0))"
    /* The one appearance setting that is a `defun' rather than a primitive: a
     * bool fits the %DO envelope exactly, so it costs a line here instead of
     * five places that can drift. Takes its argument the way the other setting
     * writers do — one required value — which also keeps it out of the M-x
     * list, since that publishes the zero-argument functions. */
    " (defun zemacs::set-scroll-past-end (on)"
    "   (zemacs::%do \"scroll-past-end\" nil (if on 1 0) 0)"
    "   on)"
    /* The unnamed register, which `p' pastes from. */
    " (defun zemacs::copy-region (beg end &optional linewise)"
    "   (zemacs::%do (if linewise \"yank-lines\" \"yank\") nil beg end))"
    " (defun zemacs::set-register (text &optional linewise)"
    "   (zemacs::%do \"set-register\" text (if linewise 1 0) 0))"
    " (defun zemacs::paste (&optional after) (zemacs::%do \"paste\" nil (if after 1 0) 0))"
    /* The three backends core has none of. These names are *also* built-in
     * action verbs, so a key bound to \"dired\" reaches core rather than this
     * function — they do the same thing, and this is the spelling Lisp can
     * call. */
    " (defun zemacs::magit (verb) (zemacs::%do \"git\" verb 0 0))"
    /* dired also takes a directory where the other two take only a verb:
     * `(dired \"/tmp\")' is the obvious spelling and was the one thing Lisp
     * could not do — every verb acts on the directory dired is already in, and
     * nothing from here could put it in one. The two are told apart on the far
     * side, next to the verb list, rather than by sniffing the string here. */
    " (defun zemacs::dired (verb-or-directory)"
    "   (zemacs::%do \"dired\" verb-or-directory 0 0))"
    " (defun zemacs::terminal (verb) (zemacs::%do \"term\" verb 0 0))"
    " (defun zemacs::find-file-at (hit) (zemacs::%do \"open-at\" hit 0 0))"

    /* --- lisp-api: buffers with no file, and calling a built-in verb ------ */
    /* `create-buffer' is idempotent by name: called twice it shows the buffer
     * and does not stack a second one, which is what lets a command that fills
     * a buffer be written as "make it, then rewrite it". It is applied on the
     * spot — unlike `find-file', which lands a frame later — so the very next
     * form already sees the new buffer, and that is the whole reason a
     * `*Messages*' command can be five lines of Lisp. */
    /* NIL means plain text. Separate from `set-major-mode' on purpose: a mode is
     * what Lisp dispatches on, a language is what tree-sitter colours, and a
     * buffer can want one without the other. */
    " (defun zemacs::set-language (&optional language)"
    "   (zemacs::%do \"set-language\" (and language (string language)) 0 0))"
    /* The writer beside `buffer-read-only-p', which until now could only ever
     * report on a *generated* buffer — one whose text is a rendering of state.
     * This is the other reason a buffer refuses an edit: a mode has decided the
     * document is being read rather than written, which is a claim about a real
     * file and one the mode's exit hook takes back with `(set-buffer-read-only
     * nil)'. `org-frozen-mode' is the first customer.
     *
     * Per *buffer*, so a split showing a frozen document beside a source file
     * lets you type in the source file. */
    " (defun zemacs::set-buffer-read-only (&optional (on t))"
    "   (zemacs::%do \"set-read-only\" nil (if on 1 0) 0)"
    "   on)"
    /* What, at the end of a line, means the next line is *inside* something —
     * `'(":")' for Python, `'("{" "(" "[")' for the C family, NIL for only
     * copying the previous line's indent. `Enter', `o' and `O' read it.
     *
     * Here rather than in `runtime/library.lisp' because it is a *mode setting*:
     * `%wrap-setting-primitives' in `modes/modes.lisp' takes the `fdefinition'
     * of every primitive in `*mode-settings*' as that file loads, and
     * `library.lisp' loads after it. Every one of its siblings — `set-tab-width',
     * `set-text-width' — is defined here for the same reason, and this is the
     * one of them that needed no C: the envelope carries a string, and a list
     * of openers joined by spaces is one.
     *
     * One argument and a list, not `&rest': the wrapper calls every setting
     * primitive with exactly one value. */
    " (defun zemacs::set-indent-openers (openers)"
    "   (zemacs::%do \"indent-openers\" (format nil \"~{~a~^ ~}\" openers) 0 0)"
    "   nil)"
    /* Emacs' `inhibit-read-only', and the reason a *renderer* mode is usable at
     * all: read-only has to mean "the user cannot type here", not "nothing can
     * ever write here". A maths curriculum is displayed frozen and is still the
     * buffer a transcribed handwritten answer is written into, and a problem's
     * status is toggled in place — both are `replace-region', both from Lisp,
     * and both would die on the guard in `Editor::apply'.
     *
     * It lifts a *claim* and never a kind: `(buffer-read-only-p)' answers
     * `:claimed' for a buffer a mode froze and `:generated' for dired, magit,
     * the dashboard and a terminal, and only the first is put back. A generated
     * buffer has no document behind it — the next refresh would throw the edit
     * away — so making one writable is not a favour, and this refuses to.
     *
     * UNWIND-PROTECT and not a SETF pair: a body that throws must not leave the
     * document editable, which is the whole failure mode this exists to have.
     *
     * ponytail: there is a window. The flag lives on the editor behind a mutex
     * that is released between primitives, so a keystroke arriving from the
     * application thread while BODY runs sees the buffer unfrozen. Emacs has the
     * same shape and gets away with it by being single-threaded. Closing it
     * means the *command* carrying its own permission rather than the editor
     * holding a mode — one more field on the write envelope — and nothing has
     * been bitten yet: BODY is a `replace-region' or two, and the window is the
     * microseconds between them. */
    " (defun zemacs::call-with-inhibited-read-only (fn)"
    "   (let ((claimed (eq (zemacs::buffer-read-only-p) :claimed)))"
    "     (unwind-protect"
    "          (progn (when claimed (zemacs::set-buffer-read-only nil))"
    "                 (funcall fn))"
    "       (when claimed (zemacs::set-buffer-read-only t)))))"
    " (defmacro zemacs::with-inhibited-read-only (&body body)"
    "   \"Run BODY able to write to a buffer a mode has frozen.\""
    "   `(zemacs::call-with-inhibited-read-only (lambda () ,@body)))"
    " (defun zemacs::create-buffer (name &optional language)"
    "   (zemacs::%do \"create-buffer\" (string name) 0 0)"
    "   (when language (zemacs::set-language language))"
    "   name)"
    /* NIL (or nothing) kills the live buffer. The last one cannot be killed and
     * says so; nothing is confirmed, because asking is policy and
     * `(buffer-modified-p)' is a reader. */
    " (defun zemacs::kill-buffer (&optional name)"
    "   (let ((i (if name (zemacs::buffer-index name) 0)))"
    "     (cond (i (zemacs::%do \"kill-buffer\" nil i 0) t)"
    "           (t (zemacs::message (format nil \"no such buffer: ~a\" name)) nil))))"
    /* Run a built-in verb by name — the same resolution `M-x' and every key
     * binding do, so `(call-command \"magit-status\")' is exactly what pressing
     * the key bound to it does. A name that is not a built-in verb falls through
     * to `(name)' in the image, which is also what `M-x' does. */
    " (defun zemacs::call-command (name) (zemacs::%do \"call\" (string name) 0 0))"
    /* One keystroke to the shell, spelled the way `key-bindings' reports one:
     * \"a\" \"SPC\" \"C-c\" \"<esc>\" \"M-<bs>\". Dropped when no shell is
     * running, which is what a key aimed at something that is not there is. */
    " (defun zemacs::term-send-key (key) (zemacs::%do \"term-key\" (string key) 0 0))"
    /* The which-key panel, one \"KEY LABEL\" row per call and NIL to clear it.
     * The renderer draws the rows in a grid above the status line and splits
     * each at its first space; a normalised key token never has one.
     *
     * Not a prompt, which is the whole point: a prompt owns the next keystroke
     * and which-key exists to help you choose one. Retiring the panel is core's
     * job — it empties it as soon as no key sequence is pending — so nothing
     * here has to remember to. */
    " (defun zemacs::which-key-row (&optional row)"
    "   (zemacs::%do \"which-key\" (and row (string row)) 0 0))"
    /* corfu: the in-buffer completion popup, which-key's sibling one level down.
     * `completion-show' says where it hangs and which row is lit; NIL takes it
     * down. `completion-row' fills it, NIL empties it — which-key's idiom
     * exactly, and for which-key's reason: one string per call.
     *
     * The rows survive a re-`show' at the *same* anchor and are dropped when it
     * moves, so cycling the selection costs one call rather than one per
     * candidate, and a new word cannot inherit the last one's list.
     *
     * `completion-at' is the reader, and the pair is not symmetric on purpose:
     * the editor hides a popup point has walked away from without telling
     * anybody, so what you set and what is on screen are different questions.
     * Ask before inserting a candidate. */
    " (defun zemacs::completion-show (&optional at (index 0))"
    "   (zemacs::%do \"completion-show\" nil (if at at -1) index))"
    " (defun zemacs::completion-row (&optional row)"
    "   (zemacs::%do \"completion-row\" (and row (string row)) 0 0))"
    /* The documentation panel beside the list, one line per call and NIL to
     * empty it. About the *selected* candidate rather than about the list, so
     * it is resent on every move — a docstring is a dozen short lines and the
     * candidates are a hundred, which is the whole reason these are two verbs
     * with two lifetimes rather than one payload. */
    " (defun zemacs::completion-doc (&optional line)"
    "   (zemacs::%do \"completion-doc\" (and line (string line)) 0 0))"
    " (defun zemacs::completion-at () (zemacs::%query \"completion-at\" 0 0))"
    " (pushnew \"completion-at\" zemacs::*readers* :test #'string=)"
    /* avy: hand the *next keystroke* to FUNCTION, as `(FUNCTION "a")'. No
     * argument stops waiting.
     *
     * The key arrives spelled the way `key-bindings' spells one — "a", "SPC",
     * "C-c", "<esc>" — so what counts as a label is decided up here and core
     * keeps no label table it could disagree with. One key: the editor forgets
     * before it calls, so nothing in the image can leave the keyboard captured,
     * and a key that names no label is a cancel the caller is *told* about
     * rather than a silent fall-through with the labels still on screen.
     *
     * Not `read-string' / `completing-read', which are the other way to get a
     * keystroke and are the wrong one: those open a *prompt*, and a prompt draws
     * a minibuffer and owns the keyboard. avy is one key pressed against the
     * document. `runtime/modes/avy.lisp' is the caller. */
    " (defun zemacs::grab-key (&optional function)"
    "   (zemacs::%do \"grab-key\""
    "                (and function (string-downcase (string function))) 0 0))"
    /* The one verb a scene needs. PAGE is a *printed* node — `(block :pad 48
     * (text (run \"hi\")))' — because a scene crosses the boundary as source,
     * like every other structure; NIL takes the page down and gives the buffer
     * its text back. `crates/lisp/src/scene.rs' is the reader on the other side
     * and `docs/gui.org' has the grammar.
     *
     * The name a config writes is `scene-set', in `runtime/gui.lisp', beside the
     * builders that print the argument and beside the click table it has to
     * retire when a new page goes up — none of which belongs in a C string
     * literal. This is the floor under it: a runtime that failed to load leaves
     * the verb reachable, exactly as `%save-file' and `%image-file' are reachable
     * without the wrappers written over them. */
    " (defun zemacs::%scene-set (&optional page) (zemacs::%do \"scene-set\" page 0 0))"
    /* --- end of the lisp-api block --------------------------------------- */

    /* --- buffers -------------------------------------------------------- */
    " (defun zemacs::buffer-names () (mapcar #'first (zemacs::buffer-info)))"
    " (defun zemacs::buffer-index (name)"
    "   (position (string name) (zemacs::buffer-info) :key #'first :test #'string=))"
    " (defun zemacs::switch-to-buffer (name)"
    "   (let ((i (zemacs::buffer-index name)))"
    "     (cond (i (zemacs::%do \"switch-buffer\" nil i 0) t)"
    "           (t (zemacs::message (format nil \"no such buffer: ~a\" name)) nil))))"
    /* Unlike Emacs' version this genuinely *shows* the other buffer for as long
     * as BODY runs: the editor has one live buffer, and switching is the only
     * way to reach another one's text. Two locks per switch, so a keystroke can
     * land while BODY is running and would be typed into the wrong buffer —
     * keep BODY short, and prefer it for reading rather than for editing.
     *
     * ponytail: buffers are matched by name, so two files with the same base
     * name are indistinguishable and the first one wins. The upgrade is a
     * reader keyed by buffer id, which needs the ids exposed first. */
    " (defmacro zemacs::with-current-buffer (name &body body)"
    "   (let ((old (gensym)) (want (gensym)))"
    "     `(let ((,old (zemacs::buffer-name)) (,want ,name))"
    "        (when (zemacs::switch-to-buffer ,want)"
    "          (unwind-protect (progn ,@body)"
    "            (zemacs::switch-to-buffer ,old))))))"

    /* --- position and text ---------------------------------------------- */
    /* A marker and not an integer, so an edit inside BODY does not land the
     * cursor a few characters off where it started. */
    " (defmacro zemacs::save-excursion (&body body)"
    "   (let ((m (gensym)))"
    "     `(let ((,m (zemacs::point-marker)))"
    "        (unwind-protect (progn ,@body)"
    "          (zemacs::goto-marker ,m)"
    "          (zemacs::delete-marker ,m)))))"
    /* A rope counts the empty line after a trailing newline, so `line-count' on
     * "a\nb\n" is 3 — right for the gutter and wrong for anything iterating.
     * Dropping it here is what keeps every line-oriented command from having to
     * remember. */
    " (defun zemacs::buffer-lines ()"
    "   (let ((n (zemacs::line-count)))"
    "     (when (and (plusp n) (string= (zemacs::line-string n) \"\")) (decf n))"
    "     (loop for i from 1 to n collect (zemacs::line-string i))))"
    " (defun zemacs::goto-line (n) (zemacs::goto-char (zemacs::line-start n)))"
    " (defun zemacs::beginning-of-line () (zemacs::goto-char (zemacs::line-start)))"
    " (defun zemacs::end-of-line () (zemacs::goto-char (zemacs::line-end)))"
    /* An empty range replaced by TEXT: an insert that does not move point and
     * cannot be interrupted halfway, unlike `goto-char' then `insert'. */
    " (defun zemacs::insert-at (pos text) (zemacs::replace-region pos pos text))"
    " (defun zemacs::delete-line (&optional line)"
    "   (zemacs::delete-region (zemacs::line-start line)"
    "                          (min (zemacs::point-max) (1+ (zemacs::line-end line)))))"
    /* Literal string search, in the buffer's own offsets. No regexps: ECL ships
     * no regexp engine, and CL's `search' is what there is.
     *
     * A bare `search' and no conversion either side of it, which is the whole
     * dividend of `buffer-string' answering characters: an index into the text
     * *is* an editor offset, and a pattern typed into a form is the same kind of
     * string as the buffer it is looked for in. There were two helpers here,
     * `%byte-index' and `%char-index', because neither of those was true. */
    " (defun zemacs::search-forward (pattern &optional start)"
    "   (search (string pattern) (zemacs::buffer-string)"
    "           :start2 (min (or start (zemacs::point)) (zemacs::point-max))))"
    " (defun zemacs::search-backward (pattern &optional start)"
    "   (search (string pattern) (zemacs::buffer-string) :from-end t"
    "           :end2 (min (or start (zemacs::point)) (zemacs::point-max))))"
    /* Built in Lisp and applied in *one* call, which is the whole lesson: N
     * separate edits would be N undo steps with N windows for a keystroke to
     * land in. Answers how many were replaced. */
    " (defun zemacs::replace-all (from to)"
    "   (let ((text (zemacs::buffer-string)) (n (length from)) (hits 0))"
    "     (if (zerop n)"
    "         0"
    "         (let ((out (with-output-to-string (s)"
    "                      (let ((i 0))"
    "                        (loop for hit = (search from text :start2 i)"
    "                              while hit"
    "                              do (write-string text s :start i :end hit)"
    "                                 (write-string to s)"
    "                                 (incf hits)"
    "                                 (setf i (+ hit n)))"
    "                        (write-string text s :start i))))"
    "               (was (zemacs::point)))"
    "           (when (plusp hits)"
    "             (zemacs::replace-region 0 (zemacs::point-max) out)"
    "             (zemacs::goto-char (min was (zemacs::point-max))))"
    "           hits))))"

    /* --- hooks ------------------------------------------------------------ */
    /* Emacs' `add-hook', with Emacs' argument order: the hook variable named as
     * a symbol, then the function to run.
     *
     * It is here, below every Lisp file, rather than in `modes.lisp' with the
     * two hooks the editor actually reports, and the one line that binds an
     * unbound HOOK is the whole reason. Every file that wanted a hook used to
     * open with `(defvar *point-moved-functions* nil)' before its PUSHNEW, not
     * because it declared the list but because it could not be sure it was not
     * the first file to mention it: `modes.lisp' may not have loaded, or may
     * not exist in this image at all. Six copies of that line, each with a
     * comment explaining it, and the comment was right every time. A helper
     * that predates every `load' turns all of it into one call.
     *
     * PUSHNEW and not PUSH, so re-reading a config does not stack a second copy
     * of the same function on a list that is walked per keystroke. */
    " (defun zemacs::add-hook (hook function)"
    "   (unless (boundp hook) (set hook nil))"
    "   (pushnew function (symbol-value hook))"
    "   function)"

    /* --- introspection --------------------------------------------------- */
    " (defun zemacs::where-is (command)"
    "   (let ((name (string-downcase (string command))))"
    "     (remove-if-not (lambda (b) (string= (third b) name))"
    "                    (zemacs::key-bindings))))"

    /* Exported by name rather than in the DEFPACKAGE above, for the same reason
     * the readers are: two lists that have to agree will eventually not. */
    " (dolist (n '(\"UNDO\" \"REDO\" \"SPLIT-WINDOW-RIGHT\" \"SPLIT-WINDOW-BELOW\""
    "              \"DELETE-WINDOW\" \"OTHER-WINDOW\" \"SELECT-WINDOW\" \"NEW-FRAME\""
    "              \"DELETE-FRAME\" \"SELECT-FRAME\" \"SCROLL-LINES\""
    "              \"SET-SCROLL-PAST-END\" \"COPY-REGION\""
    "              \"SET-REGISTER\" \"PASTE\" \"MAGIT\" \"DIRED\" \"TERMINAL\""
    "              \"FIND-FILE-AT\" \"BUFFER-NAMES\" \"BUFFER-INDEX\""
    "              \"SWITCH-TO-BUFFER\" \"WITH-CURRENT-BUFFER\" \"SAVE-EXCURSION\""
    "              \"GOTO-LINE\" \"BEGINNING-OF-LINE\" \"END-OF-LINE\" \"INSERT-AT\""
    "              \"DELETE-LINE\" \"SEARCH-FORWARD\" \"SEARCH-BACKWARD\""
    "              \"REPLACE-ALL\" \"WHERE-IS\" \"ADD-HOOK\" \"BUFFER-LINES\""
    "              \"LINE-START\" \"LINE-END\""
    "              \"LINE-STRING\" \"MESSAGES\" \"SYNTAX-COLOR\""
    /* lisp-api */
    "              \"CREATE-BUFFER\" \"KILL-BUFFER\" \"SET-LANGUAGE\""
    "              \"SET-BUFFER-READ-ONLY\" \"CALL-WITH-INHIBITED-READ-ONLY\""
    "              \"SET-INDENT-OPENERS\""
    "              \"WITH-INHIBITED-READ-ONLY\""
    "              \"CALL-COMMAND\" \"TERM-SEND-KEY\" \"GRAB-KEY\"))"
    "   (export (intern n \"ZEMACS\") \"ZEMACS\")))";

/* --- overlays ------------------------------------------------------------ */
/* The Lisp face of `zemacs_core::overlay'. Two primitives underneath it
 * (MAKE-OVERLAY, LATEX-PREVIEW) and everything else is %DO and %QUERY, which is
 * the whole argument for building overlays this way round.
 *
 * The property list lives *here*, in the image, and Rust is told only about the
 * handful of properties it has to draw. That is not a shortcut — it is what
 * makes an overlay able to carry an arbitrary Lisp value at all: an avy hint's
 * target window, a flymake diagnostic, a closure. None of those could survive
 * the trip through C as printed source, and none of them has to.
 *
 * The drawn ones, in the order the CASE below takes them: `face', `background'
 * and `display' replace or recolour the cells a range covers; `image' puts a
 * bitmap over them; `scale', `weight' and `slant' say what *type* they are set
 * in; `line-background', `line-prefix', `gutter' and `fold' are about the lines
 * the range touches rather than about its cells. `help-echo' is the odd one and
 * is sent down for a reason of its own — it draws nothing, and its reader is the
 * mouse. Everything else a config puts on an overlay stops here.
 *
 * ponytail: an overlay the editor deletes on its own — because an edit swallowed
 * the text it was about — leaves its plist behind, a few conses per stale entry.
 * `delete-overlay' and `remove-overlays' prune what they remove, which covers
 * every deliberate case. To prune the rest:
 *
 *   (maphash (lambda (id p) (declare (ignore p))
 *              (unless (overlay-position id)
 *                (remhash id *overlay-properties*)))
 *            *overlay-properties*)
 *
 * ...which is only honest for the live buffer, since a handle from another one
 * answers NIL too. Hence not doing it automatically. */
static const char *OVERLAY_FORM =
    "(progn"
    " (defparameter zemacs::*overlay-properties* (make-hash-table)"
    "   \"Overlay handle -> property plist. Everything the renderer does not"
    " draw lives only here, so a property may be any Lisp object at all.\")"
    /* Keywords, so `'face', `:face' and a symbol read in another package are
     * the same property. GETF tests with EQL and would otherwise let a config
     * set something it could never read back. */
    " (defun zemacs::%overlay-key (prop)"
    "   (intern (string-upcase (string prop)) \"KEYWORD\"))"
    " (defun zemacs::%overlay-face (value)"
    "   (and value (string-downcase (string value))))"
    /* The two colour properties take a *face name* from `face-list', not an RGB
     * triple: the theme already owns the vocabulary, so an overlay recoloured
     * by `load-theme' costs nothing and no second colour table exists. NIL
     * clears the attribute. */
    " (defun zemacs::overlay-put (ov prop value)"
    "   (let ((key (zemacs::%overlay-key prop)))"
    "     (setf (getf (gethash ov zemacs::*overlay-properties*) key) value)"
    "     (case key"
    "       (:face (zemacs::%do \"overlay-face\" (zemacs::%overlay-face value) ov 0))"
    "       (:background"
    "        (zemacs::%do \"overlay-background\" (zemacs::%overlay-face value) ov 0))"
    "       (:display"
    "        (zemacs::%do \"overlay-display\" (and value (string value)) ov 0))"
    "       (:image (zemacs::%do \"overlay-image\" nil ov (or value 0)))"
    /* --- typesetting -----------------------------------------------------
     *
     * `scale', `weight' and `slant' are what make a heading *look* like one
     * rather than like a coloured line of text, and they are the reason the
     * renderer now opens more than one font face. A multiplier, not a point
     * size: text follows `font-size' and a heading follows the text.
     *
     * Sent as a percentage because the write envelope carries integers, and it
     * costs nothing — the renderer snaps a size to one of a handful of steps
     * anyway, which is what bounds its glyph cache. `(overlay-put ov 'scale
     * 1.6)' and `... 1.5)' are the same face and deliberately so. */
    "       (:scale"
    "        (zemacs::%do \"overlay-scale\" nil ov"
    "                     (if value (round (* 100 value)) 0)))"
    /* Emacs' `:weight' and `:slant', spelled the same and with the same three
     * answers: `bold'/`italic', `normal', and NIL for "no opinion" — which is
     * not the same as `normal', since `normal' overrides an overlay underneath
     * and NIL leaves it alone. */
    "       (:weight (zemacs::%do \"overlay-weight\" (zemacs::%overlay-face value) ov 0))"
    "       (:slant (zemacs::%do \"overlay-slant\" (zemacs::%overlay-face value) ov 0))"
    /* --- the properties about *lines* ------------------------------------
     *
     * Everything above is about the cells a range covers. These three are
     * about the lines it touches, whole.
     *
     * `line-background' is a band the width of the pane, which is what
     * `background' cannot be: a background paints the cells, so a source block
     * drawn with one is a stripe as ragged as its own text. `line-prefix' is
     * Emacs' property of that name with its `wrap-prefix' folded in — the
     * continuation rows of a quoted paragraph get the bar too — and it is
     * drawn in the same overlay's `face', so one overlay says the whole
     * thing. */
    "       (:line-background"
    "        (zemacs::%do \"overlay-line-background\" (zemacs::%overlay-face value) ov 0))"
    "       (:line-prefix"
    "        (zemacs::%do \"overlay-line-prefix\" (and value (string value)) ov 0))"
    /* Emacs' fringe, in the spare column the line numbers already reserve. The
     * distinction from `line-prefix' is the one worth reading: a prefix moves
     * the line right by its own width, which is right for a quote bar over a
     * whole passage and wrong for a mark on one line in fifty — that line then
     * sits a column out from the code around it and its indentation reads as
     * broken. `gutter' draws in the margin and moves nothing. */
    "       (:gutter"
    "        (zemacs::%do \"overlay-gutter\" (and value (string value)) ov 0))"
    /* Emacs' `help-echo', and the only property in this CASE that draws
     * nothing: it is what the pointer resting on this overlay says. Sent down
     * rather than left in the plist above — which is where every undrawn
     * property stops — because the reader is the *mouse*, and the mouse is in
     * Rust: a motion event asking the image "what is under the pointer" would
     * be a round trip on the one path that fires once per pixel. Here it is a
     * scan of the overlays on one line, in core, costing nothing per pixel and
     * a few nanoseconds per row crossed.
     *
     * A string, like every other payload here, and NIL takes it off. The full
     * value stays in the plist above as well, so `overlay-get' still answers
     * whatever was put — this arm is the copy the pointer can reach. */
    "       (:help-echo"
    "        (zemacs::%do \"overlay-help-echo\" (and value (string value)) ov 0))"
    /* The line property that changes how many rows there are: the lines after
     * the overlay's first one stop occupying rows at all. The renderer does
     * not draw them and `j' steps over them, which is code folding — and
     * everything about *what* to fold is up here, in Lisp. */
    "       (:fold (zemacs::%do \"overlay-fold\" nil ov (if value 1 0)))))"
    "   value)"
    " (defun zemacs::overlay-get (ov prop)"
    "   (getf (gethash ov zemacs::*overlay-properties*)"
    "         (zemacs::%overlay-key prop)))"
    " (defun zemacs::delete-overlay (ov)"
    "   (remhash ov zemacs::*overlay-properties*)"
    "   (zemacs::%do \"delete-overlay\" nil ov 0))"
    /* `(ID START END)' each, oldest first — which is the order the renderer
     * resolves them in, so this is also priority order. */
    " (defun zemacs::overlays-in (beg end) (zemacs::%query \"overlays-in\" beg end))"
    " (defun zemacs::overlays-at (pos) (zemacs::overlays-in pos (1+ pos)))"
    " (defun zemacs::overlay-position (ov) (zemacs::%query \"overlay-position\" ov 0))"
    " (defun zemacs::overlay-start (ov) (car (zemacs::overlay-position ov)))"
    " (defun zemacs::overlay-end (ov) (cdr (zemacs::overlay-position ov)))"
    /* Emacs' blunt instrument, and blunt here too: it takes *every* overlay in
     * the range, whoever made it. To remove only your own, walk `overlays-in'
     * and `delete-overlay' the ones your property marks — which is what
     * `org-latex-preview' does. */
    " (defun zemacs::remove-overlays (&optional beg end)"
    "   (let ((beg (or beg (zemacs::point-min))) (end (or end (zemacs::point-max))))"
    "     (dolist (o (zemacs::overlays-in beg end))"
    "       (remhash (first o) zemacs::*overlay-properties*))"
    "     (zemacs::%do \"remove-overlays\" nil beg end)))"
    /* `(START END DISPLAY-P)' per fragment, in order. The one reader answered
     * outside `query.rs' — org's scanner is downstream of core. */
    " (defun zemacs::latex-fragments () (zemacs::%query \"latex-fragments\" 0 0))"
    /* The other one: `(FIRST-LINE LAST-LINE)' per structural range in the live
     * buffer, outermost first, from the same tree-sitter parse that colours it.
     * *Lines*, because a fold hides whole lines and the caller was going to
     * convert an offset back to one anyway.
     *
     * Every range, not a chosen one — which of the nested ranges to fold is
     * policy, and policy is `org-fold.lisp''s. A buffer with no grammar answers
     * `()', which is the honest structure of plain text. */
    " (defun zemacs::fold-ranges () (zemacs::%query \"fold-ranges\" 0 0))"
    /* And the third: the directories the editor has opened a file in, newest
     * first, off the list `crates/project' persists. `project-switch' is a
     * `completing-read' over this and nothing more, which is the shape every
     * picker in the image should have — the two that cannot are the project's
     * *files* and *directories*, where the list is tens of thousands long and
     * goes into the prompt without passing through here. See
     * `runtime/plugins/project.lisp'. */
    " (defun zemacs::project-recent () (zemacs::%query \"project-recent\" 0 0))"
    /* A reader is a noun, not a command: keep it out of the M-x list the same
     * way every other reader is kept out. */
    " (pushnew \"latex-fragments\" zemacs::*readers* :test #'string=)"
    " (pushnew \"fold-ranges\" zemacs::*readers* :test #'string=)"
    " (pushnew \"project-recent\" zemacs::*readers* :test #'string=)"
    /* The other producer of an `image' id: a *file*, rather than a LaTeX run.
     * WIDTH is in ems and may be fractional, which is why the primitive
     * underneath takes hundredths — the same percentage `overlay-scale' sends
     * for the same reason. NIL means "as authored".
     *
     * Answers NIL when the file cannot be read or parsed, with the reason
     * already in the status line, so `(when (image-file p) ...)' is the idiom —
     * exactly `latex-preview''s contract. */
    " (defun zemacs::image-file (path &optional width)"
    "   (zemacs::%image-file (namestring path)"
    "                        (if width (round (* 100 width)) 0)))"
    " (export (intern \"IMAGE-FILE\" \"ZEMACS\") \"ZEMACS\")"
    /* --- folding ---------------------------------------------------------
     *
     * Five functions and no new mechanism: a fold *is* an overlay carrying
     * `fold', so it moves with the text, dies with it, and is undone by the
     * same `delete-overlay' as anything else. What is deliberately not here is
     * any opinion about *what* is foldable — an org subtree, a `defun', a
     * brace block — because that is the policy, and policy is Lisp's.
     * `runtime/modes/org-fold.lisp' is the first such policy and lives
     * entirely on top of these. */
    " (defun zemacs::fold-region (beg end)"
    "   \"Hide every line after BEG's, through END's. Answers the overlay.\""
    "   (let ((ov (zemacs::make-overlay beg end)))"
    "     (zemacs::overlay-put ov 'fold t)"
    "     ov))"
    /* Only *our* overlays, unlike `remove-overlays': an org buffer is full of
     * `display' overlays for its bullets and folding must not eat them. */
    " (defun zemacs::folds-in (beg end)"
    "   \"Every fold overlapping BEG..END, as handles, oldest first.\""
    "   (let ((out nil))"
    "     (dolist (o (zemacs::overlays-in beg end) (nreverse out))"
    "       (when (zemacs::overlay-get (first o) 'fold) (push (first o) out)))))"
    " (defun zemacs::folded-p (&optional pos)"
    "   \"The innermost fold covering POS (default point), or NIL.\""
    "   (let ((pos (or pos (zemacs::point))))"
    "     (first (last (zemacs::folds-in pos (1+ pos))))))"
    " (defun zemacs::unfold-region (beg end)"
    "   \"Take out every fold overlapping BEG..END. Answers how many.\""
    "   (let ((folds (zemacs::folds-in beg end)))"
    "     (dolist (ov folds (length folds)) (zemacs::delete-overlay ov))))"
    " (defun zemacs::unfold-all ()"
    "   \"Open every fold in the buffer.\""
    "   (zemacs::unfold-region (zemacs::point-min) (zemacs::point-max)))"
    " (dolist (n '(\"MAKE-OVERLAY\" \"OVERLAY-PUT\" \"OVERLAY-GET\" \"DELETE-OVERLAY\""
    "              \"OVERLAYS-IN\" \"OVERLAYS-AT\" \"OVERLAY-POSITION\""
    "              \"OVERLAY-START\" \"OVERLAY-END\" \"REMOVE-OVERLAYS\""
    "              \"FOLD-REGION\" \"FOLDS-IN\" \"FOLDED-P\" \"UNFOLD-REGION\""
    "              \"UNFOLD-ALL\""
    "              \"LATEX-PREVIEW\" \"LATEX-FRAGMENTS\" \"HIGHLIGHT\""
    "              \"FOLD-RANGES\" \"PROJECT-RECENT\""
    "              \"*OVERLAY-PROPERTIES*\"))"
    "   (export (intern n \"ZEMACS\") \"ZEMACS\")))";

/* --- lisp-api: prompts whose answer comes back here ----------------------- */
/* `read-string' and `completing-read', which is what most interactive commands
 * in a real config are built out of. Every other prompt has its destination
 * fixed in core — a file is opened, a command is run — and this one hands the
 * reply to a Lisp closure instead.
 *
 * **It is a continuation, not a blocking read**, and it has to be: the Lisp
 * thread evaluates one queued form at a time, so a primitive that waited for a
 * keystroke would stop the image — every mode hook, every key bound to a Lisp
 * command, every `M-x' — until the user answered. That is precisely the elisp
 * behaviour `docs/threading.org' exists to avoid.
 *
 * The route is the one `runtime/rpc.lisp' already uses for a JSON-RPC reply, and
 * deliberately the same shape rather than a second mechanism:
 *
 *   1. the closure is parked in a table under a fresh id;
 *   2. `%DO \"read-from-minibuffer\"' carries that id into core, which opens a
 *      prompt of kind `Lisp';
 *   3. accepting or cancelling it produces `CallLisp', the app hands the form to
 *      this thread, and `%prompt-reply' calls the closure parked under the id.
 *
 * No foreign thread ever calls into ECL and nothing blocks the editor. The cost
 * is the one every continuation has: your callback cannot return a value to
 * whoever asked. Write the rest of the command inside it.
 *
 * A table rather than a single variable even though the editor has exactly one
 * prompt slot: an id means a stale reply is *dropped* rather than delivered to
 * whoever asked most recently, which is the failure a single variable would have
 * if Lisp opened a second prompt over the first.
 *
 * ponytail: a Lisp prompt replaced by another prompt before it is answered never
 * gets a reply, so its closure stays in the table — a few conses, and only
 * reachable by opening a picker from inside a `read-string'. The sweep is
 * `(clrhash *prompt-continuations*)'. */
static const char *PROMPT_FORM =
    "(progn"
    " (defparameter zemacs::*prompt-continuations* (make-hash-table)"
    "   \"Prompt id -> the closure waiting for that answer.\")"
    " (defparameter zemacs::*prompt-previews* (make-hash-table)"
    "   \"Prompt id -> the closure shown each candidate as the highlight moves.\")"
    " (defparameter zemacs::*prompt-next-id* 0)"
    " (defun zemacs::%prompt-park (k)"
    "   (setf (gethash (incf zemacs::*prompt-next-id*)"
    "                  zemacs::*prompt-continuations*) k)"
    "   zemacs::*prompt-next-id*)"
    /* Called by the editor, on this thread, with the answer or NIL. Everything
     * is inside a HANDLER-CASE for the same reason `%rpc-event' is: a callback
     * that signals must cost you that one answer, not the editor. Answers NIL so
     * `eval-string' has nothing to echo over the command's own message. */
    " (defun zemacs::%prompt-reply (id answer)"
    "   (let ((k (gethash id zemacs::*prompt-continuations*)))"
    "     (remhash id zemacs::*prompt-continuations*)"
    "     (remhash id zemacs::*prompt-previews*)"
    "     (when k"
    "       (handler-case (funcall k answer)"
    "         (error (e) (zemacs::message"
    "                     (format nil \"prompt handler error: ~a\" e))))))"
    "   nil)"
    /* Called by the editor every time the highlight moves in a previewing
     * picker, with the candidate now under it. The other half of
     * `%prompt-reply', and guarded the same way: a preview that signals must
     * cost you that one frame's preview rather than the editor.
     *
     * Its own table rather than the continuation's, because the two have
     * different lifetimes in the one direction that matters — the preview fires
     * many times and the reply exactly once, and the reply is what clears both.
     *
     * Deliberately *not* cleared here on a NIL: cancelling is delivered as a
     * reply, so the callback that has been previewing gets the NIL through its
     * own `%prompt-reply' and is the thing that puts back whatever it changed. */
    " (defun zemacs::%prompt-preview (id candidate)"
    "   (let ((k (gethash id zemacs::*prompt-previews*)))"
    "     (when k"
    "       (handler-case (funcall k candidate)"
    "         (error (e) (zemacs::message"
    "                     (format nil \"preview handler error: ~a\" e))))))"
    "   nil)"
    /* CALLBACK is called with the string typed, or NIL if the prompt was
     * cancelled — so `(when answer ...)' is the idiom, as it is for every other
     * optional in this API. Answers the id, as `rpc-request' does. */
    " (defun zemacs::read-string (label callback)"
    "   (let ((id (zemacs::%prompt-park callback)))"
    "     (zemacs::%do \"read-from-minibuffer\" (string label) id 0)"
    "     id))"
    /* The same, with a candidate list and the fuzzy matcher every other picker
     * uses. Nothing is required to match: with no hit the answer is what was
     * typed, which is Emacs' `require-match' NIL and the useful default.
     *
     * The candidates go over as *one* newline-joined string, which is one lock
     * and one crossing however long the list is. It used to be one `%do' each,
     * with a note here saying that was fine for the few hundred a command offers
     * and that a longer list wanted another route; the route is `prompt-items',
     * and it is the same one `project-find-file' takes to fill a picker with
     * fifty thousand paths. A candidate is a row in a one-line-per-row popup, so
     * it has nowhere to put a newline of its own. */
    /* PREVIEW is optional and is what makes this a *live* picker: it is called
     * with each candidate as the highlight moves over it, and with NIL never —
     * cancelling arrives at CALLBACK, which is where the undo belongs, since
     * only the caller knows what it was showing before the prompt opened.
     *
     * `load-theme' and `choose-font' are the two that pass one, and they are
     * the argument for the feature: a list of theme names tells you nothing
     * about the themes. */
    " (defun zemacs::completing-read (label candidates callback &optional preview)"
    "   (let ((id (zemacs::%prompt-park callback)))"
    "     (when preview (setf (gethash id zemacs::*prompt-previews*) preview))"
    "     (zemacs::%do \"read-from-minibuffer\" (string label) id"
    "                  (if preview 2 1))"
    "     (when candidates"
    "       (zemacs::%do \"prompt-items\""
    "                    (format nil \"~{~a~^~%~}\" (mapcar #'string candidates))"
    "                    0 0))"
    "     id))"
    " (dolist (n '(\"READ-STRING\" \"COMPLETING-READ\" \"*PROMPT-CONTINUATIONS*\""
    "              \"*PROMPT-PREVIEWS*\"))"
    "   (export (intern n \"ZEMACS\") \"ZEMACS\")))";

/* --- end of the lisp-api block -------------------------------------------- */

static void defprim(const char *name, cl_objectfn_fixed fn, int narg) {
  ecl_def_c_function(ecl_make_symbol(name, "ZEMACS"), fn, narg);
}

static int booted = 0;

/* cl_boot arms an exit hook that runs cl_shutdown on whichever thread calls
 * exit() — for an embedded image that is the *main* thread, which ECL knows
 * nothing about. It then fails a pthread_getspecific and pthread_exit()s the
 * main thread, hanging the process on every quit. Registering this straight
 * after cl_boot means it runs first (exit hooks are LIFO) and marks the image
 * shut down, so ECL's own hook returns immediately. */
static void disarm_ecl_shutdown(void) { ecl_set_option(ECL_OPT_BOOTED, -1); }

/* Must be the first ECL call on this thread, and must happen on the thread
 * that will own the image — cl_boot registers the caller with the GC. */
void zemacs_boot(void) {
  if (booted)
    return;
  booted = 1;

  /* We are a guest in someone else's process: an editor with a GPU event loop
   * on the main thread does not want ECL stealing signal handlers. */
  ecl_set_option(ECL_OPT_TRAP_SIGSEGV, 0);
  ecl_set_option(ECL_OPT_TRAP_SIGFPE, 0);
  ecl_set_option(ECL_OPT_TRAP_SIGINT, 0);
  ecl_set_option(ECL_OPT_TRAP_SIGILL, 0);
  ecl_set_option(ECL_OPT_TRAP_SIGBUS, 0);
  ecl_set_option(ECL_OPT_TRAP_SIGPIPE, 0);

  char arg0[] = "zemacs";
  char *argv[] = {arg0, NULL};
  cl_boot(1, argv);
  atexit(disarm_ecl_shutdown);

  cl_safe_eval(ecl_read_from_cstring(PACKAGE_FORM), ECL_NIL, ECL_NIL);

  defprim("SET-FONT-SIZE", (cl_objectfn_fixed)f_set_font_size, 1);
  defprim("SET-BACKGROUND", (cl_objectfn_fixed)f_set_background, 3);
  defprim("SET-FOREGROUND", (cl_objectfn_fixed)f_set_foreground, 3);
  defprim("SET-SYNTAX-COLOR", (cl_objectfn_fixed)f_set_syntax_color, 4);
  defprim("SET-FACE-STYLE", (cl_objectfn_fixed)f_set_face_style, 3);
  defprim("RESET-FACES", (cl_objectfn_fixed)f_reset_faces, 0);
  defprim("SET-LINE-NUMBERS", (cl_objectfn_fixed)f_set_line_numbers, 1);
  defprim("SET-TAB-WIDTH", (cl_objectfn_fixed)f_set_tab_width, 1);
  defprim("SET-TEXT-WIDTH", (cl_objectfn_fixed)f_set_text_width, 1);
  defprim("SET-MODELINE-RELIEF", (cl_objectfn_fixed)f_set_modeline_relief, 1);
  defprim("SET-MODELINE-PAD", (cl_objectfn_fixed)f_set_modeline_pad, 1);
  defprim("SET-LINE-OVERFLOW", (cl_objectfn_fixed)f_set_line_overflow, 1);
  defprim("SET-RELATIVE-LINE-NUMBERS",
          (cl_objectfn_fixed)f_set_relative_line_numbers, 1);
  defprim("SET-MAJOR-MODE", (cl_objectfn_fixed)f_set_major_mode, 1);
  defprim("SET-NO-GUTTER-MODES", (cl_objectfn_fixed)f_set_no_gutter_modes, 1);
  defprim("SET-MINOR-MODE", (cl_objectfn_fixed)f_set_minor_mode, 2);
  defprim("MESSAGE", (cl_objectfn_fixed)f_message, 1);
  defprim("QUIT", (cl_objectfn_fixed)f_quit, 0);
  defprim("DASHBOARD-BANNER", (cl_objectfn_fixed)f_dashboard_banner, 1);
  defprim("DASHBOARD-LOGO", (cl_objectfn_fixed)f_dashboard_logo, 1);
  defprim("CLEAR-DASHBOARD-ITEMS", (cl_objectfn_fixed)f_clear_dashboard_items,
          0);
  /* Four arguments, and `%'-prefixed because of the fourth: `defprim' binds a
     fixed arity, so growing this one would have broken every config that calls
     `dashboard-item' with the three it has always taken. library.lisp defines the
     three-or-four-argument `dashboard-item' over it, which is the same shape
     `%save-file' and `%make-marker' already have. */
  defprim("%DASHBOARD-ITEM", (cl_objectfn_fixed)f_dashboard_item, 4);
  defprim("DEFINE-KEY", (cl_objectfn_fixed)f_define_key, 3);
  defprim("FIND-FILE", (cl_objectfn_fixed)f_find_file, 1);
  defprim("%SAVE-FILE", (cl_objectfn_fixed)f_save_file, 1);
  defprim("SHOW-DASHBOARD", (cl_objectfn_fixed)f_show_dashboard, 0);
  defprim("INSERT", (cl_objectfn_fixed)f_insert, 1);
  defprim("SET-COMPLETION-STYLE", (cl_objectfn_fixed)f_set_completion_style, 1);
  defprim("CLEAR-COMMANDS", (cl_objectfn_fixed)f_clear_commands, 0);
  defprim("REGISTER-COMMAND", (cl_objectfn_fixed)f_register_command, 1);
  defprim("%QUERY", (cl_objectfn_fixed)f_query, 3);
  defprim("%DO", (cl_objectfn_fixed)f_do, 4);
  defprim("GOTO-CHAR", (cl_objectfn_fixed)f_goto_char, 1);
  defprim("DELETE-REGION", (cl_objectfn_fixed)f_delete_region, 2);
  defprim("REPLACE-REGION", (cl_objectfn_fixed)f_replace_region, 3);
  defprim("%MAKE-MARKER", (cl_objectfn_fixed)f_make_marker, 2);
  defprim("SET-MARKER", (cl_objectfn_fixed)f_set_marker, 2);
  defprim("DELETE-MARKER", (cl_objectfn_fixed)f_delete_marker, 1);
  defprim("SET-EVIL-STATE", (cl_objectfn_fixed)f_set_evil_state, 1);
  defprim("OPEN-PROMPT", (cl_objectfn_fixed)f_open_prompt, 1);
  /* --- JSON-RPC subprocesses. All internal: the names a config uses live in
   * `runtime/rpc.lisp', which is where the protocol is written. --- */
  defprim("%RPC-START", (cl_objectfn_fixed)f_rpc_start, 3);
  defprim("%RPC-SEND", (cl_objectfn_fixed)f_rpc_send, 4);
  defprim("%RPC-RESPOND", (cl_objectfn_fixed)f_rpc_respond, 4);
  defprim("%RPC-STOP", (cl_objectfn_fixed)f_rpc_stop, 1);
  defprim("%JSON-QUOTE", (cl_objectfn_fixed)f_json_quote, 1);
  /* --- end of the JSON-RPC defprims --- */
  /* --- overlays: the only ones that have to answer with a value --- */
  defprim("MAKE-OVERLAY", (cl_objectfn_fixed)f_make_overlay, 2);
  defprim("LATEX-PREVIEW", (cl_objectfn_fixed)f_latex_preview, 1);
  defprim("%IMAGE-FILE", (cl_objectfn_fixed)f_image_file, 2);
  /* Not an overlay itself — it is what a mode turns *into* overlays, which is
   * why it sits with them rather than with the readers: `%QUERY' takes two
   * integers and this takes two strings. */
  defprim("HIGHLIGHT", (cl_objectfn_fixed)f_highlight, 2);
  /* --- end of the overlay defprims --- */

  cl_safe_eval(ecl_read_from_cstring(HELPERS_FORM), ECL_NIL, ECL_NIL);
  cl_safe_eval(ecl_read_from_cstring(QUERIES_FORM), ECL_NIL, ECL_NIL);
  /* Depends on nothing, so its position here is only about reading order. */
  cl_safe_eval(ecl_read_from_cstring(PATHS_FORM), ECL_NIL, ECL_NIL);
  /* Last: everything in it is written in terms of the two above. */
  cl_safe_eval(ecl_read_from_cstring(LIBRARY_FORM), ECL_NIL, ECL_NIL);
  /* ...and this one in terms of all three: `*readers*' comes from QUERIES_FORM. */
  cl_safe_eval(ecl_read_from_cstring(OVERLAY_FORM), ECL_NIL, ECL_NIL);
  /* lisp-api: needs only %DO, but goes last so a reload reads in file order. */
  cl_safe_eval(ecl_read_from_cstring(PROMPT_FORM), ECL_NIL, ECL_NIL);
}

/* Strings are self-evaluating, so `(zemacs::f "...")` needs no QUOTE. Both
 * entry points funnel through Lisp helpers that HANDLER-CASE their body, and
 * cl_safe_eval is the backstop for anything that escapes that.
 *
 * Both decode, and `zemacs_eval' is the one that matters: the source is READ
 * inside `eval-string', so a base string would have `(message "λ")' from a
 * keybinding arrive as the two Latin-1 characters its UTF-8 spells and put
 * `Î»' in the status line. A path is decoded for the smaller version of the
 * same reason — an accented directory in `~' is a real thing. */

void zemacs_load_init(const char *path) {
  cl_object form =
      cl_list(2, ecl_make_symbol("LOAD-INIT", "ZEMACS"), utf8_string(path));
  cl_safe_eval(form, ECL_NIL, ECL_NIL);
}

void zemacs_eval(const char *src) {
  cl_object form =
      cl_list(2, ecl_make_symbol("EVAL-STRING", "ZEMACS"), utf8_string(src));
  cl_safe_eval(form, ECL_NIL, ECL_NIL);
}
