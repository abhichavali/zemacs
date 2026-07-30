/* The C half of the ECL bridge.
 *
 * ECL's API is macros all the way down: ECL_NIL, ECL_BASE_STRING_P, cl_object
 * tagging, cl_list's varargs. None of that survives a hand-written Rust
 * `extern "C"` block, so the boundary lives here — Rust only ever sees plain
 * scalars and NUL-terminated UTF-8, in both directions.
 *
 * Every zemacs primitive is a *side effect*: it converts its Lisp arguments and
 * calls back into Rust, which pushes an EditorCommand down a channel. Nothing
 * here reads or writes editor state.
 */

#include <ecl/ecl.h>
#include <stdlib.h>
#include <string.h>

/* Implemented in Rust (src/lib.rs). A NULL string means "absent". */
extern void rs_set_font_size(double size);
extern void rs_set_background(double r, double g, double b);
extern void rs_set_foreground(double r, double g, double b);
extern void rs_set_syntax_color(const char *face, double r, double g, double b);
extern void rs_set_line_numbers(int on);
extern void rs_set_tab_width(long n);
extern void rs_set_modeline_relief(long n);
extern void rs_set_modeline_pad(long n);
extern void rs_message(const char *text);
extern void rs_quit(void);
extern void rs_dashboard_banner(const char *text);
extern void rs_clear_dashboard_items(void);
extern void rs_dashboard_item(const char *key, const char *label, const char *action);
extern void rs_define_key(const char *mode, const char *keys, const char *command);
extern void rs_find_file(const char *path);
extern void rs_save_file(const char *path);
extern void rs_show_dashboard(void);
extern void rs_insert(const char *text);
extern void rs_set_completion_style(const char *style);
extern void rs_clear_commands(void);
extern void rs_register_command(const char *name);

/* --- Lisp -> C conversions ---------------------------------------------- */

/* Owned UTF-8 copy of any object's PRINC form; NULL for NIL. Going through
 * PRINC rather than demanding a string means #\f, "f", 'find-file and #P"/x"
 * all work, so callers never have to think about types.
 *
 * Base strings are copied byte-for-byte: `zemacs_eval` hands ECL raw UTF-8 as
 * a base string, so passing the bytes back untouched round-trips exactly.
 * Extended (unicode) strings — what `load` produces from a UTF-8 file — get
 * encoded here. */
static char *dup_utf8(cl_object x) {
  if (x == ECL_NIL)
    return NULL;
  cl_object s = cl_princ_to_string(x);

  if (ECL_BASE_STRING_P(s)) {
    cl_index n = s->base_string.fillp;
    char *buf = (char *)malloc((size_t)n + 1);
    if (!buf)
      return NULL;
    memcpy(buf, s->base_string.self, (size_t)n);
    buf[n] = '\0';
    return buf;
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

static cl_object f_set_line_numbers(cl_object on) {
  rs_set_line_numbers(on != ECL_NIL);
  return ECL_NIL;
}

static cl_object f_set_tab_width(cl_object n) {
  rs_set_tab_width((long)ecl_to_fixnum(n));
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

static cl_object f_clear_dashboard_items(void) {
  rs_clear_dashboard_items();
  return ECL_NIL;
}

static cl_object f_dashboard_item(cl_object key, cl_object label,
                                  cl_object action) {
  char *k = dup_utf8_or_empty(key);
  char *l = dup_utf8_or_empty(label);
  char *a = dup_utf8_or_empty(action);
  rs_dashboard_item(k, l, a);
  free(k);
  free(l);
  free(a);
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

/* --- Boot --------------------------------------------------------------- */

/* Creating the package has to happen before ecl_def_c_function, which interns
 * into it. Exporting the names here is what makes `zemacs:message` read. */
static const char *PACKAGE_FORM =
    "(defpackage \"ZEMACS\" (:use \"CL\")"
    " (:export \"SET-FONT-SIZE\" \"SET-BACKGROUND\" \"SET-FOREGROUND\""
    "          \"SET-SYNTAX-COLOR\" \"SET-LINE-NUMBERS\" \"SET-TAB-WIDTH\""
    "          \"SET-MODELINE-RELIEF\" \"SET-MODELINE-PAD\""
    "          \"MESSAGE\" \"QUIT\" \"DASHBOARD-BANNER\""
    "          \"CLEAR-DASHBOARD-ITEMS\" \"DASHBOARD-ITEM\" \"DEFINE-KEY\""
    "          \"FIND-FILE\" \"SAVE-FILE\" \"SHOW-DASHBOARD\" \"INSERT\""
    "          \"SET-COMPLETION-STYLE\" \"CLEAR-COMMANDS\""
    "          \"REGISTER-COMMAND\"))";

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
  defprim("SET-LINE-NUMBERS", (cl_objectfn_fixed)f_set_line_numbers, 1);
  defprim("SET-TAB-WIDTH", (cl_objectfn_fixed)f_set_tab_width, 1);
  defprim("SET-MODELINE-RELIEF", (cl_objectfn_fixed)f_set_modeline_relief, 1);
  defprim("SET-MODELINE-PAD", (cl_objectfn_fixed)f_set_modeline_pad, 1);
  defprim("MESSAGE", (cl_objectfn_fixed)f_message, 1);
  defprim("QUIT", (cl_objectfn_fixed)f_quit, 0);
  defprim("DASHBOARD-BANNER", (cl_objectfn_fixed)f_dashboard_banner, 1);
  defprim("CLEAR-DASHBOARD-ITEMS", (cl_objectfn_fixed)f_clear_dashboard_items,
          0);
  defprim("DASHBOARD-ITEM", (cl_objectfn_fixed)f_dashboard_item, 3);
  defprim("DEFINE-KEY", (cl_objectfn_fixed)f_define_key, 3);
  defprim("FIND-FILE", (cl_objectfn_fixed)f_find_file, 1);
  defprim("%SAVE-FILE", (cl_objectfn_fixed)f_save_file, 1);
  defprim("SHOW-DASHBOARD", (cl_objectfn_fixed)f_show_dashboard, 0);
  defprim("INSERT", (cl_objectfn_fixed)f_insert, 1);
  defprim("SET-COMPLETION-STYLE", (cl_objectfn_fixed)f_set_completion_style, 1);
  defprim("CLEAR-COMMANDS", (cl_objectfn_fixed)f_clear_commands, 0);
  defprim("REGISTER-COMMAND", (cl_objectfn_fixed)f_register_command, 1);

  cl_safe_eval(ecl_read_from_cstring(HELPERS_FORM), ECL_NIL, ECL_NIL);
}

/* Strings are self-evaluating, so `(zemacs::f "...")` needs no QUOTE. Both
 * entry points funnel through Lisp helpers that HANDLER-CASE their body, and
 * cl_safe_eval is the backstop for anything that escapes that. */

void zemacs_load_init(const char *path) {
  cl_object form = cl_list(2, ecl_make_symbol("LOAD-INIT", "ZEMACS"),
                           ecl_make_simple_base_string((char *)path, -1));
  cl_safe_eval(form, ECL_NIL, ECL_NIL);
}

void zemacs_eval(const char *src) {
  cl_object form = cl_list(2, ecl_make_symbol("EVAL-STRING", "ZEMACS"),
                           ecl_make_simple_base_string((char *)src, -1));
  cl_safe_eval(form, ECL_NIL, ECL_NIL);
}
