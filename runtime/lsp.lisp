;;;; LSP — the eglot equivalent, written in Lisp on top of `rpc.lisp'.
;;;;
;;;; Nothing in Rust knows what a language server is. This file is the whole
;;;; client: the handshake, which server serves which mode, what to do with a
;;;; reply, where a diagnostic goes. That is deliberate — it is the layer a
;;;; config wants to bend, and the image is where bending costs nothing.
;;;;
;;;; *Adding a language server is one line here and nothing anywhere else:*
;;;;
;;;;   (lsp-register-server 'go-mode "gopls")
;;;;   (lsp-register-server 'rust-mode "rust-analyzer")
;;;;   (lsp-register-server 'c-mode "clangd" "--background-index")
;;;;
;;;; What is bound to what:
;;;;
;;;;   (lsp)                      start a server for the live buffer
;;;;   (lsp-stop) (lsp-restart)
;;;;   (lsp-goto-definition)      jump to the definition under the cursor
;;;;   (lsp-diagnostics-at-point) echo the diagnostic on this line
;;;;   (lsp-list-diagnostics)     every diagnostic, in a buffer
;;;;   (lsp-status)               what is running
;;;;   (lsp-diagnostics &optional path)   the data, for anything that draws it
;;;;   (lsp-complete)             ask for completions here, and pop them up
;;;;   (lsp-complete-next) (lsp-complete-previous)
;;;;   (lsp-complete-accept) (lsp-complete-abort)
;;;;
;;;; ---------------------------------------------------------------------------
;;;; What the editor tells us, and what it costs
;;;;
;;;; One signal: `after-change-hook', which the application fires whenever the
;;;; document's revision moves. It carries no *delta*, so
;;;; `textDocument/didChange' sends the **whole buffer** every time.
;;;;
;;;; ponytail: full text rather than incremental sync. The delta this used to say
;;;; did not exist now does — `after-edit-hook' carries (START OLD-END NEW-END
;;;; TEXT) — so what is left is the work of turning it into a `contentChanges'
;;;; range, plus the bookkeeping to fall back to a full send whenever an edit is
;;;; missed. Until somebody does that this costs one buffer copy per revision,
;;;; which is what Emacs paid for years and is not felt below a few hundred KB.
;;;; The completion popup below deliberately does *not* use the delta either, and
;;;; says why where it reads the buffer.
;;;;
;;;; The escaping of that copy is the one thing that is *not* in Lisp
;;;; (`%json-quote'), because doing it a character at a time in the image on
;;;; every keystroke is exactly the case the boundary rule exists for.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; After-change
;;;
;;; The application calls `after-change-hook' the way it calls `X-hook': by name,
;;; guarded with `fboundp'. A list rather than a single function so that this
;;; file and your config can both hang something off it — the one hook the
;;; editor now reports about the *document* should not be first-come-first-served.
;;;
;;; ponytail: this belongs in the standard library next to `define-derived-mode'
;;; rather than in the LSP client, and should move there the day something else
;;; wants it. It is here because the LSP client is the only thing that does.

(defvar *after-change-functions* nil
  "Functions called with no arguments after any change to the live buffer.")

(defun after-change-hook ()
  "Called by the editor whenever the document's revision moves.

Errors are swallowed per function: this fires on every keystroke, and a broken
hook must cost you that hook rather than the ability to type."
  (dolist (f *after-change-functions*)
    (ignore-errors (funcall f))))

;;; ---------------------------------------------------------------------------
;;; Paths and URIs
;;;
;;; Every string that crosses the shim is UTF-8 *bytes* in a base string, so
;;; percent-encoding one character at a time is byte-level encoding and comes
;;; out right — which is the one place that convention is a help rather than a
;;; ceiling.

(defun %uri-unreserved-p (code)
  "True for the bytes a `file:' URI may carry literally: ASCII letters and
digits, `-._~' and the separator itself."
  (or (<= 65 code 90) (<= 97 code 122) (<= 48 code 57)
      (member code '(45 46 95 126 47))))

(defun lsp-path-uri (path)
  "PATH as a `file:' URI, percent-encoded."
  (with-output-to-string (out)
    (write-string "file://" out)
    (loop for ch across (string path)
          for code = (char-code ch)
          do (if (%uri-unreserved-p code)
                 (write-char ch out)
                 (format out "%~2,'0X" code)))))

(defun lsp-uri-path (uri)
  "The filesystem path inside a `file:' URI, or NIL for anything else — a
server is allowed to answer with a location in a jar, and jumping to one is not
something this editor can do.

Built as a base string on purpose: a decoded `%C3%A9' is the byte 0xC3, and only
a base string carries bytes back across the shim unchanged."
  (when (and (stringp uri) (>= (length uri) 7) (string= "file://" uri :end2 7))
    (let* ((raw (subseq uri 7))
           (n (length raw))
           (out (make-array n :element-type 'base-char :fill-pointer 0))
           (i 0))
      (loop while (< i n)
            do (let ((ch (char raw i)))
                 (cond ((and (char= ch #\%) (<= (+ i 3) n))
                        (let ((v (ignore-errors
                                  (parse-integer raw :start (1+ i) :end (+ i 3)
                                                     :radix 16))))
                          (cond (v (vector-push (code-char v) out) (incf i 3))
                                (t (vector-push ch out) (incf i)))))
                       (t (vector-push ch out) (incf i)))))
      (subseq out 0 (fill-pointer out)))))

(defun %file-directory (path)
  "PATH's directory, trailing slash included."
  (let ((i (position #\/ path :from-end t)))
    (if i (subseq path 0 (1+ i)) "./")))

(defun %parent-directory (dir)
  "DIR's parent, or NIL at the filesystem root. DIR ends in a slash."
  (let ((i (position #\/ dir :from-end t :end (max 0 (1- (length dir))))))
    (when i (subseq dir 0 (1+ i)))))

(defparameter *lsp-root-markers*
  '(".git" "compile_commands.json" "CMakeLists.txt" "pyproject.toml"
    "setup.py" "setup.cfg" "Cargo.toml" "go.mod" "package.json")
  "Files that mean \"the project starts here\". A VCS root is first because it
is the answer that is right when a repository contains several build files.")

(defun lsp-project-root (path)
  "The directory a server should be started in for PATH.

Walks up looking for a marker and falls back to the file's own directory, which
is what a scratch file outside any project wants."
  (or (loop for dir = (%file-directory path) then (%parent-directory dir)
            while dir
            when (some (lambda (m)
                         (ignore-errors
                          (probe-file (concatenate 'string dir m))))
                       *lsp-root-markers*)
              return dir)
      (%file-directory path)))

(defvar *lsp-roots* (make-hash-table :test #'equal)
  "PATH -> its project root, remembered.

Not an optimisation for its own sake: `lsp-ensure' runs on every keystroke and
`lsp-project-root' walks the filesystem, so without this every character typed
would `probe-file' its way to the root of the disk. A project that grows a
`.git' while the editor is open keeps the root it had — `M-x lsp-restart' after
`git init' is the whole workaround.")

(defun %lsp-root-for (path)
  (or (gethash path *lsp-roots*)
      (setf (gethash path *lsp-roots*) (lsp-project-root path))))

;;; ---------------------------------------------------------------------------
;;; Which server for which mode
;;;
;;; The whole registry, and the whole reason a third language is a one-line
;;; change. DEFVAR so a reload does not drop what a config registered before
;;; this file was read; the two shipped entries below are re-registered anyway.

(defvar *lsp-servers* (make-hash-table :test #'equal)
  "Major mode name -> a plist of :PROGRAM and :ARGS.")

;;; A mode is a string to the editor and a symbol to whoever is writing the
;;; config. Spelled out here rather than borrowing `%mode-name' from
;;; `modes.lisp': this file needs nothing else from the mode system, and being
;;; loadable on its own is worth two lines.
(defun %lsp-mode-name (mode) (string-downcase (string mode)))

(defun lsp-register-server (mode program &rest args)
  "Use PROGRAM as the language server for MODE."
  (setf (gethash (%lsp-mode-name mode) *lsp-servers*)
        (list :program (string program) :args args))
  (%lsp-mode-name mode))

(defvar *lsp-language-ids*
  '(("python-mode" . "python") ("c-mode" . "c") ("rust-mode" . "rust")
    ("javascript-mode" . "javascript") ("json-mode" . "json")
    ("toml-mode" . "toml") ("lisp-mode" . "commonlisp"))
  "Major mode name -> the `languageId' a server expects. A mode that is not
here sends its own name with `-mode' cut off, which is right often enough that
the table only holds the exceptions.")

(defun lsp-language-id (mode)
  (or (cdr (assoc mode *lsp-language-ids* :test #'string=))
      (let ((i (search "-mode" mode :from-end t)))
        (if i (subseq mode 0 i) mode))))

;;; ---------------------------------------------------------------------------
;;; Sessions
;;;
;;; One per (mode, project root) pair, which is eglot's rule and the right one:
;;; two Python projects open at once want two `pylsp's, and two files in the
;;; same project want one.

(defvar *lsp-sessions* (make-hash-table :test #'equal)
  "KEY -> a plist: :conn :mode :root :state :queue :opened :versions.
:state is :STARTING until `initialized' has gone out, then :READY.")

(defvar *lsp-conn-keys* (make-hash-table)
  "CONN -> KEY, so an incoming message can find its session.")

(defun %lsp-key (mode root) (concatenate 'string mode " " root))
(defun %lsp-get (key prop) (getf (gethash key *lsp-sessions*) prop))
(defun %lsp-set (key prop value) (setf (getf (gethash key *lsp-sessions*) prop) value))

(defun lsp-session-for-buffer ()
  "The session KEY serving the live buffer, or NIL. Does not start one."
  (let ((path (buffer-file-name))
        (mode (major-mode)))
    (when (and path (gethash mode *lsp-servers*))
      (let ((key (%lsp-key mode (%lsp-root-for path))))
        (when (gethash key *lsp-sessions*) key)))))

;;; The client's half of the handshake. Deliberately small: everything declared
;;; here is something this editor can actually do, and a capability you claim
;;; and do not honour is a server sending you work you throw away.
;;;
;;; ponytail: no `positionEncoding' negotiation. LSP counts a column in UTF-16
;;; code units and this editor counts characters, so the two agree exactly on
;;; ASCII and disagree on a line with an accent before the cursor. Negotiating
;;; `utf-8' would only trade one mismatch (code units) for another (bytes); the
;;; real fix is a reader that answers a line's UTF-16 length, which is a line in
;;; `query.rs' and worth adding when a non-ASCII source file misplaces a jump.
(defun %lsp-capabilities ()
  (jobj "general" (jobj "markdown" (jobj "parser" "none"))
        "textDocument"
        (jobj "synchronization" (jobj "didSave" t "willSave" :false)
              "definition" (jobj "linkSupport" t)
              ;; Completion, and every claim in here is a *refusal*, which is
              ;; the useful half of this negotiation: a server told we do
              ;; snippets sends `$1' placeholders we would insert literally, and
              ;; one told we resolve lazily sends items with no `detail' until
              ;; asked. Both are real features and neither is built, so both are
              ;; declined and the server sends us plain text it has finished
              ;; filling in. `documentationFormat' is empty for the reason
              ;; `markdown parser none' is: nothing renders it.
              "completion"
              (jobj "completionItem"
                    (jobj "snippetSupport" :false
                          "insertReplaceSupport" :false
                          "resolveSupport" :false
                          "documentationFormat" :empty-array)
                    "contextSupport" t)
              "publishDiagnostics" (jobj "relatedInformation" :false))
        "workspace" (jobj "workspaceFolders" :false
                          "configuration" :false)))

(defun %lsp-start (mode root)
  "Spawn the server for MODE at ROOT and begin the handshake. Answers the
session KEY, or NIL if the program could not be started."
  (let* ((spec (gethash mode *lsp-servers*))
         (key (%lsp-key mode root))
         (conn (and spec
                    (rpc-start (getf spec :program)
                               :args (getf spec :args)
                               :cwd root
                               :name (getf spec :program)
                               :on-notify #'%lsp-notification
                               :on-request #'%lsp-request
                               :on-exit #'%lsp-exit))))
    (when conn
      (setf (gethash key *lsp-sessions*)
            (list :conn conn :mode mode :root root :state :starting
                  :queue nil :opened nil :versions nil))
      (setf (gethash conn *lsp-conn-keys*) key)
      (rpc-request conn "initialize"
                   (jobj "processId" nil
                         "clientInfo" (jobj "name" "zemacs")
                         "rootUri" (lsp-path-uri root)
                         "rootPath" root
                         "workspaceFolders" nil
                         "capabilities" (%lsp-capabilities))
                   (lambda (result error)
                     (%lsp-initialized key result error)))
      key)))

(defun %lsp-initialized (key result error)
  "The reply to `initialize'. Nothing may be sent to a server before
`initialized' goes out, which is why everything until now was queued.

RESULT is read for exactly one thing — whether the server completes at all.
Everything else it advertises we either already assumed or do not implement, and
a client that inspected capabilities it never acts on would be writing down its
own wishes."
  (let ((conn (%lsp-get key :conn)))
    (cond
      (error
       (message (format nil "lsp: initialize failed: ~a" (jget error "message")))
       ;; The child is still running — a server that refuses the handshake does
       ;; not exit on its own, and leaving it would leak a process per attempt.
       (rpc-stop conn)
       (%lsp-forget key))
      (t
       (rpc-notify conn "initialized" :empty-object)
       ;; `completionProvider' is an *object* when the server completes and
       ;; absent when it does not, so its presence is the whole test. Recorded
       ;; rather than asked per keystroke, and honoured rather than ignored:
       ;; a server with no provider answers `textDocument/completion' with
       ;; MethodNotFound, and a popup that produced an error message on every
       ;; word typed would be worse than one that never appears.
       (%lsp-set key :completion (and (jget result "capabilities" "completionProvider") t))
       (%lsp-set key :state :ready)
       ;; In order: the didOpen that started all this has to precede the
       ;; didChanges that piled up behind it.
       (dolist (thunk (reverse (%lsp-get key :queue))) (ignore-errors (funcall thunk)))
       (%lsp-set key :queue nil)
       (message (format nil "lsp: ~a ready in ~a"
                        (%lsp-get key :mode) (%lsp-get key :root)))))))

(defun %lsp-send (key method params)
  "Send a notification, or park it until the handshake finishes.

PARAMS is built *now* even when the send is deferred: it carries the buffer text
as it is at this moment, and a thunk that read it later would send whatever the
buffer had become."
  (let ((conn (%lsp-get key :conn)))
    (if (eq (%lsp-get key :state) :ready)
        (rpc-notify conn method params)
        (%lsp-set key :queue
                  (cons (lambda () (rpc-notify conn method params))
                        (%lsp-get key :queue))))))

(defun %lsp-forget (key)
  "Drop the session without talking to the child — for a server that has already
died or never came up."
  (let ((conn (%lsp-get key :conn)))
    (when conn (remhash conn *lsp-conn-keys*))
    (remhash key *lsp-sessions*)))

;;; ---------------------------------------------------------------------------
;;; Document synchronisation

(defun %lsp-version (key path)
  "The next version number for PATH, remembered per session as LSP requires
them to be monotonic per document."
  (let* ((table (%lsp-get key :versions))
         (v (1+ (or (cdr (assoc path table :test #'string=)) 0))))
    (%lsp-set key :versions
              (cons (cons path v) (remove path table :key #'car :test #'string=)))
    v))

(defun %lsp-did-open (key path)
  (%lsp-send key "textDocument/didOpen"
             (jobj "textDocument"
                   (jobj "uri" (lsp-path-uri path)
                         "languageId" (lsp-language-id (%lsp-get key :mode))
                         "version" (%lsp-version key path)
                         "text" (buffer-string))))
  (%lsp-set key :opened (cons path (%lsp-get key :opened))))

(defun %lsp-did-change (key path)
  (%lsp-send key "textDocument/didChange"
             (jobj "textDocument" (jobj "uri" (lsp-path-uri path)
                                        "version" (%lsp-version key path))
                   ;; One change covering everything. See the ponytail note at
                   ;; the top of this file for why it is not a range.
                   "contentChanges" (jarr (jobj "text" (buffer-string))))))

(defun lsp-ensure ()
  "Make sure the live buffer's server is running and has the buffer's text.

Called from `after-change-hook', so this runs on every keystroke in a buffer
with a server, and on nothing else: a buffer with no file, a generated buffer,
and a mode with no server registered all fall out on the first test."
  (let ((path (buffer-file-name))
        (mode (major-mode)))
    (when (and path (not (buffer-read-only-p)) (gethash mode *lsp-servers*))
      (let* ((root (%lsp-root-for path))
             (key (%lsp-key mode root)))
        (unless (gethash key *lsp-sessions*) (setf key (%lsp-start mode root)))
        (when key
          (if (member path (%lsp-get key :opened) :test #'string=)
              (%lsp-did-change key path)
              (%lsp-did-open key path))))))
  ;; NIL on purpose, here and in every command below. `eval-string' echoes the
  ;; value of the last form into the status line, so a command that fell out of
  ;; a `rpc-notify' would flash a bare `T' — or, worse, a request id — every
  ;; time you pressed the key.
  nil)

(pushnew 'lsp-ensure *after-change-functions*)

(defun lsp-did-save ()
  "Tell the server the live buffer was saved."
  (let ((key (lsp-session-for-buffer))
        (path (buffer-file-name)))
    (when (and key path)
      (%lsp-send key "textDocument/didSave"
                 (jobj "textDocument" (jobj "uri" (lsp-path-uri path))))))
  nil)

;;; Advice, which in Common Lisp is `fdefinition' and needs no framework — the
;;; same mechanism `modes.lisp' uses to make a setting revert.
;;;
;;; ponytail: no text in the didSave, and the save itself lands a frame later
;;; (see `docs/threading.org'), so a server that re-reads from disk on save sees
;;; the file one turn stale. Harmless here because full-text didChange has
;;; already given it the current buffer; it would stop being harmless the day
;;; sync goes incremental.
(defvar *lsp-save-advised* nil)
(unless *lsp-save-advised*
  (setf *lsp-save-advised* t)
  (let ((inner (fdefinition 'save-file)))
    (setf (fdefinition 'save-file)
          (lambda (&optional path)
            (funcall inner path)
            (ignore-errors (lsp-did-save))))))

;;; ---------------------------------------------------------------------------
;;; Diagnostics
;;;
;;; Stored as data first and shown second, because the *showing* is about to
;;; change: overlays are being built, and flymake-style gutter and inline
;;; rendering attaches to `*lsp-diagnostics-functions*' below without this file
;;; changing at all.

(defvar *lsp-diagnostics* (make-hash-table :test #'equal)
  "Absolute path -> a list of (LINE COLUMN SEVERITY MESSAGE SOURCE).
LINE is 1-based, agreeing with `line-number'; COLUMN is 0-based, agreeing with
`column'. SEVERITY is 1 error, 2 warning, 3 information, 4 hint.")

;;; ---------------------------------------------------------------------------
;;; THE OVERLAY SEAM.
;;;
;;; Each function here is called with one argument, the path whose diagnostics
;;; changed, after `*lsp-diagnostics*' has been updated. An overlay-based
;;; renderer hangs off this and needs nothing from this file:
;;;
;;;   (push (lambda (path)
;;;           (when (string= path (buffer-file-name))
;;;             (clear-my-overlays)
;;;             (dolist (d (lsp-diagnostics path))
;;;               (make-overlay-on-line (first d) (face-for (third d))))))
;;;         *lsp-diagnostics-functions*)
;;;
;;; It exists now — see "Diagnostics in the gutter" below, which is written
;;; against this variable and nothing else. The rest of the presentation is
;;; still here beside it: a summary in the echo area when the diagnostics for
;;; the buffer you are looking at change, a command that reads out the one on
;;; the current line, and a listing buffer.
(defvar *lsp-diagnostics-functions* nil
  "Called with a path when its diagnostics change. Where flymake attaches.")

(defun lsp-diagnostics (&optional (path (buffer-file-name)))
  "Every diagnostic for PATH, as a list of (LINE COLUMN SEVERITY MESSAGE SOURCE)."
  (and path (gethash path *lsp-diagnostics*)))

(defun lsp-severity-name (n)
  (case n (1 "error") (2 "warning") (3 "info") (4 "hint") (t "note")))

(defun %lsp-publish-diagnostics (params)
  (let* ((path (lsp-uri-path (jget params "uri")))
         (rows (loop for d in (jget params "diagnostics")
                     collect (list (1+ (or (jget d "range" "start" "line") 0))
                                   (or (jget d "range" "start" "character") 0)
                                   (or (jget d "severity") 1)
                                   (or (jget d "message") "")
                                   (or (jget d "source") "")))))
    (when path
      (if rows
          (setf (gethash path *lsp-diagnostics*) rows)
          (remhash path *lsp-diagnostics*))
      (when (equal path (buffer-file-name)) (%lsp-echo-summary rows))
      (dolist (f *lsp-diagnostics-functions*) (ignore-errors (funcall f path))))))

(defun %lsp-echo-summary (rows)
  (let ((errors (count 1 rows :key #'third))
        (warnings (count 2 rows :key #'third)))
    (message (cond ((null rows) "lsp: no diagnostics")
                   (t (format nil "lsp: ~d error~:p, ~d warning~:p" errors warnings))))))

(defun lsp-diagnostics-at-point ()
  "Echo the diagnostics on the cursor's line."
  (let* ((line (line-number))
         (here (remove-if-not (lambda (d) (eql (first d) line)) (lsp-diagnostics))))
    (if here
        (message (format nil "~{~a~^ | ~}"
                         (mapcar (lambda (d)
                                   (format nil "~a: ~a~@[ [~a]~]"
                                           (lsp-severity-name (third d))
                                           (fourth d)
                                           (and (plusp (length (fifth d))) (fifth d))))
                                 here)))
        (message "no diagnostic on this line"))))

;;; ---------------------------------------------------------------------------
;;; Diagnostics in the gutter
;;;
;;; The first thing hung off the seam above, and it hangs off it exactly the way
;;; the sketch there says: the seam fires with a path, this repaints if that
;;; path is the one on screen, and not a line above this point knows it exists.
;;;
;;; A *mark in the margin* rather than a colour on the offending text, and the
;;; reason is the face list. An overlay's colours are named from `face-list' so
;;; that a theme change recolours them for free — which is the right trade
;;; everywhere else and is the wrong one here, because there is no `error' face
;;; in that list and there is no room to add one from Lisp. Colouring
;;; diagnostics would mean spending `keyword' or `string' on them with
;;; `set-syntax-color', and that changes what every keyword in every buffer
;;; looks like. Severity goes in the *shape* of the mark instead, which no theme
;;; can take away and which reads the same in a light theme and a dark one.
;;;
;;; `line-prefix' is the property that makes this one overlay rather than two:
;;; it pushes the whole line right and draws in the gap it opened, continuation
;;; rows included, so a wrapped line stays lined up under its own mark. And it
;;; is an overlay, so the mark slides down when you type a line above it and
;;; dies with the text it marked — without waiting for the server to answer
;;; again, which on a slow server is most of a second of the marks pointing at
;;; the wrong lines.
;;;
;;; ponytail: the *message* is still `SPC l e'. Putting it in the buffer wants
;;; virtual text — a string drawn after a line's end without covering anything —
;;; and `display' is emphatically not that: it replaces the cells it covers, so
;;; an inline message would eat the code it is about. The upgrade path is an
;;; `after-string' overlay property, which is a payload in `overlay.rs' and a
;;; branch in the renderer's line loop.

(defparameter *lsp-diagnostic-marks*
  '((1 . "● ") (2 . "▲ ") (3 . "› ") (4 . "· "))
  "Severity -> the mark drawn in the left margin of its line.
Severities are 1 error, 2 warning, 3 information, 4 hint, as `*lsp-diagnostics*'
records them. A `defparameter' because it is the whole of the taste here: change
the strings, or set it to NIL to stop drawing marks at all.")

(defvar *lsp-diagnostic-overlays* (make-hash-table :test #'equal)
  "Path -> the overlay handles drawn in that path's buffer.
Per path rather than one list, because an overlay belongs to exactly one buffer
and `delete-overlay' on a handle from another one is silently nothing — a single
list would strand a mark in every file you looked at and never take it off.")

(defvar *lsp-drawn-in* nil
  "The path `%lsp-draw-diagnostics' last painted. See `%lsp-redraw-on-switch'.")

(defun %lsp-diagnostic-overlay (line severity)
  "Mark LINE with SEVERITY's glyph. NIL if there is nothing there to mark."
  (let* ((beg (line-start line))
         ;; The line's own text, so the overlay is *about* what the server
         ;; complained on and dies with it. An empty line has none and
         ;; `make-overlay' answers NIL for an empty range, so fall back to the
         ;; newline itself — the one character an empty line does have, and the
         ;; case that matters, since an unclosed bracket is reported exactly
         ;; there. Only the last line of a buffer that ends without a newline
         ;; has neither, and goes unmarked.
         (end (max (line-end line) (min (1+ beg) (point-max))))
         (ov (and (> end beg) (make-overlay beg end))))
    (when ov
      (overlay-put ov 'line-prefix
                   (or (cdr (assoc severity *lsp-diagnostic-marks*)) "? "))
      ;; Not read by the renderer — it is how these are told from anyone else's
      ;; overlays, the way `folds-in' tells folds from org's bullets.
      (overlay-put ov 'lsp-diagnostic severity))
    ov))

(defun %lsp-draw-diagnostics (&optional (path (buffer-file-name)))
  "Repaint PATH's marks. Silent unless PATH is the buffer on screen.

Silent and not queued: an overlay can only be made in the live buffer, so a file
whose diagnostics arrived while you were looking elsewhere is painted when you
look at it — which is `%lsp-redraw-on-switch', below."
  (when (and path (equal path (buffer-file-name)))
    (dolist (ov (gethash path *lsp-diagnostic-overlays*))
      (ignore-errors (delete-overlay ov)))
    (setf (gethash path *lsp-diagnostic-overlays*)
          (when *lsp-diagnostic-marks*
            (loop for d in (lsp-diagnostics path)
                  for ov = (%lsp-diagnostic-overlay (first d) (third d))
                  when ov collect ov)))
    (setf *lsp-drawn-in* path)
    nil))

(push '%lsp-draw-diagnostics *lsp-diagnostics-functions*)

;;; Diagnostics arrive once; the buffer they are about goes in and out of view
;;; many times. The seam alone would therefore leave a file you came *back* to
;;; wearing whatever marks it had when you left, or none at all.
;;;
;;; `after-change-hook' fires on a buffer switch as well as on an edit — the
;;; revision moves either way — so the switch is noticed here rather than
;;; needing a hook the editor does not have. The cost on the common path, which
;;; is a keystroke, is one string compare.
(defun %lsp-redraw-on-switch ()
  (let ((path (buffer-file-name)))
    (unless (equal path *lsp-drawn-in*)
      (setf *lsp-drawn-in* path)
      (when (and path (lsp-diagnostics path))
        (%lsp-draw-diagnostics path)))))

(pushnew '%lsp-redraw-on-switch *after-change-functions*)

(defparameter *lsp-diagnostics-file*
  (zemacs-file "diagnostics.txt")
  "Where `lsp-list-diagnostics' writes its listing. A real file for the same
reason the scratch buffer is one: `find-file' is the only way to put the editor
in a different buffer.")

(defun lsp-list-diagnostics ()
  "Every diagnostic from every server, in a buffer, as `path:line:message' —
the same shape ripgrep prints, so `SPC' on a line opens it there.

ponytail: a plain file, and the editor switches to an already-open buffer rather
than re-reading it, so running this twice in a session shows the first listing.
The fix is a `revert-buffer' or a real generated buffer kind, neither of which
exists yet."
  (handler-case
      (let ((rows nil))
        (maphash (lambda (path ds)
                   (dolist (d ds)
                     (push (format nil "~a:~a:~a: ~a" path (first d)
                                   (lsp-severity-name (third d)) (fourth d))
                           rows)))
                 *lsp-diagnostics*)
        (setf rows (sort rows #'string<))
        (ensure-directories-exist *lsp-diagnostics-file*)
        (with-open-file (out *lsp-diagnostics-file*
                             :direction :output :if-exists :supersede
                             :if-does-not-exist :create
                             ;; The strings here are UTF-8 bytes in base strings
                             ;; (see `docs/boundary.org'); latin-1 writes each
                             ;; one out as the byte it already is, where utf-8
                             ;; would encode it a second time.
                             :external-format :latin-1)
          (if rows
              (dolist (r rows) (write-line r out))
              (write-line "no diagnostics" out)))
        (find-file (namestring *lsp-diagnostics-file*)))
    (error (e) (message (format nil "lsp-list-diagnostics: ~a" e)))))

;;; ---------------------------------------------------------------------------
;;; Incoming

(defun %lsp-notification (conn method params)
  (declare (ignore conn))
  (cond ((string= method "textDocument/publishDiagnostics")
         (%lsp-publish-diagnostics params))
        ;; The two every server sends and nobody has to act on. Silence rather
        ;; than a message: `pylsp' logs its whole startup this way.
        ((or (string= method "window/logMessage")
             (string= method "$/progress")
             (string= method "telemetry/event")))
        ((string= method "window/showMessage")
         (message (format nil "lsp: ~a" (jget params "message"))))))

(defun %lsp-request (conn id method params)
  "Answer a request the *server* made. We advertise almost nothing, so the only
ones that arrive are the ones a server sends regardless — and every one of them
has to be answered or the server waits forever."
  (cond ((string= method "window/workDoneProgress/create") (rpc-respond conn id nil))
        ((string= method "client/registerCapability") (rpc-respond conn id nil))
        ((string= method "workspace/configuration")
         ;; One null per requested item: "no setting, use your default".
         (rpc-respond conn id (mapcar (constantly nil) (jget params "items"))))
        (t (rpc-respond conn id nil
                        (jobj "code" -32601
                              "message" (format nil "~a is not implemented" method))))))

(defun %lsp-exit (conn report)
  (let ((key (gethash conn *lsp-conn-keys*)))
    (remhash conn *lsp-conn-keys*)
    (when key
      (remhash key *lsp-sessions*)
      (message (format nil "lsp: ~a ~a" key report)))))

;;; ---------------------------------------------------------------------------
;;; Commands

(defun lsp ()
  "Start a language server for the live buffer, or report why not."
  (let ((path (buffer-file-name))
        (mode (major-mode)))
    (cond ((null path) (message "lsp: this buffer has no file"))
          ((null (gethash mode *lsp-servers*))
           (message (format nil "lsp: no server registered for ~a" mode)))
          ((lsp-session-for-buffer) (message "lsp: already running"))
          (t (lsp-ensure))))
  nil)

(defun lsp-stop ()
  "Shut the live buffer's server down, politely and then not."
  (let ((key (lsp-session-for-buffer)))
    (if (null key)
        (message "lsp: nothing running here")
        (let ((conn (%lsp-get key :conn)))
          ;; The protocol's own sequence. `rpc-stop' writes what is queued
          ;; before it closes the pipe, so both of these reach the server.
          (dolist (path (%lsp-get key :opened))
            (rpc-notify conn "textDocument/didClose"
                        (jobj "textDocument" (jobj "uri" (lsp-path-uri path)))))
          (rpc-request conn "shutdown" nil (lambda (r e) (declare (ignore r e))))
          (rpc-notify conn "exit")
          (rpc-stop conn)
          (%lsp-forget key)
          (message (format nil "lsp: stopped ~a" key)))))
  nil)

(defun lsp-restart ()
  "Stop and start again — what you reach for after changing a server's config."
  (lsp-stop)
  (lsp-ensure)
  nil)

(defun lsp-status ()
  "What is running, and where."
  (let (rows)
    (maphash (lambda (key plist)
               (push (format nil "~a [~a]" key (getf plist :state)) rows))
             *lsp-sessions*)
    (message (if rows (format nil "lsp: ~{~a~^, ~}" rows) "lsp: nothing running"))))

(defun %lsp-goto-location (loc)
  "Jump to a Location or a LocationLink. `find-file-at' is the only way to open
a file *and* land on a line — a `goto-char' after `find-file' would move the
cursor in the buffer being left."
  (let* ((uri (or (jget loc "uri") (jget loc "targetUri")))
         (range (or (jget loc "range") (jget loc "targetSelectionRange")
                    (jget loc "targetRange")))
         (path (lsp-uri-path uri))
         (line (1+ (or (jget range "start" "line") 0))))
    (if path
        (find-file-at (format nil "~a:~a:" path line))
        (message (format nil "lsp: cannot open ~a" uri)))))

(defun lsp-goto-definition ()
  "Jump to the definition of whatever is under the cursor."
  (let ((key (lsp-session-for-buffer))
        (path (buffer-file-name)))
    (if (null key)
        (message "lsp: no server for this buffer")
        (rpc-request
         (%lsp-get key :conn) "textDocument/definition"
         (jobj "textDocument" (jobj "uri" (lsp-path-uri path))
               ;; LSP counts lines from 0 and `line-number' from 1.
               "position" (jobj "line" (1- (line-number)) "character" (column)))
         (lambda (result error)
           (cond (error (message (format nil "lsp: ~a" (jget error "message"))))
                 ((null result) (message "lsp: no definition found"))
                 ;; A Location has a `uri'; a list of them, or of LocationLinks,
                 ;; does not — take the first, which is what every editor does.
                 ((jget result "uri") (%lsp-goto-location result))
                 ((jget result "targetUri") (%lsp-goto-location result))
                 ((consp result) (%lsp-goto-location (first result)))
                 (t (message "lsp: no definition found")))))))
  nil)

;;; ---------------------------------------------------------------------------
;;; Completion at point — corfu
;;;
;;; A server has been running this whole file and there has been no way to see
;;; anything it knows while you type. This is that: `textDocument/completion',
;;; and a box of candidates hanging off the caret.
;;;
;;; **The popup is `completion-show'/`completion-row', which is which-key's
;;; mechanism one level down** — the editor draws rows the image fills in, and
;;; the editor decides when they stop being true. Not overlays, which is the
;;; answer that keeps looking right and keeps being wrong: an overlay's payload
;;; is drawn *in the flow of the line*, so ten candidates would be ten lines of
;;; document that are not in the file. Not a prompt either, and here that is the
;;; whole feature — a prompt owns the keyboard, and completion works by your
;;; going on typing into the buffer while the list narrows.
;;;
;;; ---------------------------------------------------------------------------
;;; Three clocks, none of them the keyboard's
;;;
;;; Everything hard here is timing, so it is worth being explicit about what is
;;; late and by how much.
;;;
;;; 1. *The hook is a queue turn behind the keystroke* (`docs/threading.org').
;;;    So `lsp-complete-maybe' never trusts what it was told about the edit: it
;;;    re-reads `(point)' and the text behind it and asks "what word is point in
;;;    **now**". That is also why the delta hook is not used. `after-edit-hook'
;;;    would hand us (START OLD-END NEW-END TEXT), and that describes an edit
;;;    which may already be two edits old by the time we run — whereas a read of
;;;    the buffer at hook time is current by construction. A delta is the right
;;;    signal for something reconstructing a *history* (incremental didChange
;;;    wants it); it is the wrong one for a question about the present.
;;;
;;; 2. *The server is a subprocess.* The reply is validated against the buffer
;;;    as it is **when it arrives** rather than as it was when we asked — same
;;;    file, anchor unmoved, still a word there — and the candidates are then
;;;    filtered against the prefix re-read at that moment. An answer for a word
;;;    you have stopped typing is dropped rather than shown.
;;;
;;; 3. *The editor retires the popup without telling us.* Point walks out of the
;;;    word, `Esc' leaves Insert, and core hides the box on the spot — the same
;;;    way it empties which-key, and for the same reason: the image cannot see a
;;;    keystroke resolve. So `lsp-complete-accept' asks `(completion-at)' for the
;;;    anchor instead of trusting the one it stored, and inserts nothing when the
;;;    answer is NIL. Otherwise `C-y' would complete a word whose popup is not on
;;;    screen.
;;;
;;; The insertion itself is one `replace-region' over [anchor, point), which is
;;; `docs/threading.org's rule about all-or-nothing sequences: a `delete-region'
;;; and an `insert' have a keystroke's width between them.
;;;
;;; ---------------------------------------------------------------------------
;;; Why a *prefix* match, and not the minibuffer's
;;;
;;; `crates/core/src/minibuffer.rs' has a good matcher — subsequence scoring,
;;; and now orderless on top of it — and this deliberately does not use it.
;;;
;;; In a minibuffer the query is a scratch string that is thrown away when you
;;; accept; here **the query is the document**. A candidate replaces the
;;; characters you typed, so a match that is not a prefix rewrites text you
;;; chose: with subsequence matching `str' offers `from_str', and accepting it
;;; deletes the `str' you meant and leaves a symbol you never asked for. The
;;; looseness that makes `buffer switch' find `switch-to-buffer' is exactly what
;;; you do not want three characters into a word.
;;;
;;; The second reason is that the server *already filtered*, against the position
;;; we gave it and with a real index behind it. A looser client-side matcher
;;; cannot narrow that list, only widen it back out.
;;;
;;; Case-insensitive, though, which is the one looseness that is a prefix: `str'
;;; finding `String' is a completion and not a substitution.

(defparameter *lsp-completion-minimum* 2
  "How many characters of a word must be typed before the popup appears.
One is too few — every identifier in the file matches a single letter, and the
box would be up permanently. `lsp-complete' ignores this: asking explicitly is a
statement that you want the list however short the word is.")

(defparameter *lsp-completion-limit* 20
  "How many candidates to show. Past this the popup is a directory rather than a
suggestion, and you are better off typing another character.")

(defvar *lsp-completion* nil
  "The live popup, or NIL. A plist:

  :AT      buffer offset the word starts at — the popup's anchor, and the start
           of the range a candidate replaces
  :PATH    the file it belongs to, so a reply for another one is dropped
  :ITEMS   every candidate the server sent, as (INSERT FILTER ROW)
  :SHOWN   the ones matching the prefix now, same shape, capped
  :INDEX   which of :SHOWN is highlighted")

;;; Word characters, and **ASCII on purpose** — which is the one thing in this
;;; section that has to be read before it is changed.
;;;
;;; Buffer text reaches Lisp as UTF-8 *bytes*, one character per byte
;;; (`docs/boundary.org'), while `(point)' counts real characters. Scanning back
;;; over bytes to find where a word starts therefore gives an offset in the wrong
;;; unit — except that every byte of a multi-byte sequence is >= #x80, so a scan
;;; that stops at the first non-ASCII byte has counted bytes that are each
;;; exactly one character. The anchor is correct on any line, in any encoding,
;;; without decoding anything.
;;;
;;; `utf8-text' is the alternative and is not needed here: decoding to count
;;; would then leave the prefix in *characters* while the server's candidates
;;; arrive in bytes like everything else across the shim, and the comparison
;;; would be between two different alphabets. Bytes on both sides, ASCII where it
;;; matters, and the candidate goes back into the buffer as the bytes it arrived
;;; as.
;;;
;;; ponytail: a word containing a non-ASCII letter — `naïve', a Greek variable in
;;; a Python file — has its prefix start after that letter, so the popup filters
;;; on `ve' rather than on `naïve'. A shorter prefix, never a wrong offset, so
;;; the worst case is a longer list. Upgrade path is the one `f_query' already
;;; writes up: move buffer text to characters and delete the two index
;;; conversions.

(defun %lsp-word-char-p (ch)
  "True for the ASCII characters an identifier is made of."
  (let ((c (char-code ch)))
    (or (<= 65 c 90) (<= 97 c 122) (<= 48 c 57) (= c 95))))

(defun %lsp-completion-prefix ()
  "(AT . PREFIX) for the word point sits at the end of, or NIL when it does not.

AT is a character offset and PREFIX is the buffer's own bytes — see the note
above for why those two units can be mixed here and nowhere else."
  (let* ((beg (line-start))
         (p (point))
         (text (buffer-substring beg p))
         (n (length text))
         (k 0))
    (loop while (and (< k n) (%lsp-word-char-p (char text (- n k 1))))
          do (incf k))
    (when (plusp k)
      (cons (- p k) (subseq text (- n k))))))

(defun %lsp-prefix-p (prefix candidate)
  "True when CANDIDATE starts with PREFIX, ignoring case."
  (let ((n (length prefix)))
    (and (<= n (length candidate))
         (string-equal prefix candidate :end2 n))))

(defun %lsp-completion-items (result)
  "The items in a `CompletionList', or a bare array of `CompletionItem'.

Both shapes are legal and servers ship both. A decoded object is an alist, so
`jget' answers NIL for the array case without being told which one it has —
`%json-object-p' is only needed to tell an empty list apart from an object that
happens to have no items."
  (cond ((null result) nil)
        ((jget result "items"))
        ((%json-object-p result) nil)
        ((listp result) result)))

(defun %lsp-completion-row (item)
  "(INSERT FILTER ROW) for one `CompletionItem', or NIL for one with no label.

INSERT is what goes in the buffer, FILTER is what the prefix is tested against,
and ROW is what is drawn — three fields because LSP says they can differ, and a
server that sets `filterText' has told us the label is not what to match on.

ponytail: `textEdit' is ignored, so a server whose edit range is not exactly
[anchor, point) — clangd replacing a whole call, a server that completes past a
`.' — inserts the plain label instead. Ceiling: those completions come out
slightly wrong rather than not at all. Upgrade path: read `textEdit.range',
convert its line/character pair through `line-start', and pass that range to
`replace-region' instead of the anchor. `snippetSupport' is declined in
`%lsp-capabilities' precisely so nothing arrives with `$1' in it meanwhile."
  (let* ((label (or (jget item "label") ""))
         (insert (or (jget item "insertText") label))
         (filter (or (jget item "filterText") label))
         (detail (jget item "detail")))
    (when (plusp (length label))
      (list insert filter
            ;; The detail beside the name — a type for clangd, a signature for
            ;; pylsp. Two spaces and no column, because the renderer draws one
            ;; string per row and lining them up would mean measuring cells in
            ;; the image, which is the boundary this editor keeps on the other
            ;; side. ponytail: no kind icon; upgrade path is a table from
            ;; `CompletionItemKind' to a glyph, prepended here.
            (if (and detail (plusp (length detail)))
                (format nil "~a  ~a" label detail)
                label)))))

(defun %lsp-completion-hide ()
  "Take the popup down and forget what was in it.

The *forgetting* is the whole difference between this and the bare
`completion-show' in `%lsp-completion-draw', and only three things are entitled
to it: abandoning the word, accepting a candidate, and `C-e'. A no-op when there
is nothing up, so the common keystroke in a buffer with a server — Insert mode,
no word under point — costs a NIL test rather than a command down the channel."
  (when *lsp-completion*
    (setf *lsp-completion* nil)
    (completion-show))
  nil)

(defun %lsp-completion-draw ()
  "Send the current candidates and selection to the editor.

`completion-show' first and `completion-row' after, in that order: rows land in
a popup that exists, and showing at an anchor that has not moved keeps whatever
rows are already there — so the explicit clear is what makes this a replacement
rather than an append."
  (let ((rows (getf *lsp-completion* :shown)))
    (cond
      (rows
       (completion-show (getf *lsp-completion* :at) (getf *lsp-completion* :index))
       (completion-row)
       (dolist (r rows) (completion-row (third r))))
      ;; Nothing to draw — and the state stays, in both of the ways this
      ;; happens.
      ;;
      ;; While the request is still out there is nothing to match against yet,
      ;; and dropping the plist here would throw away the very thing the reply
      ;; is checked against: typing a third character would *cancel the request
      ;; the second one triggered*, and the popup would only ever appear if you
      ;; stopped typing at exactly two letters.
      ;;
      ;; And once the items are in, a prefix that matches none of them is one
      ;; backspace away from matching again — so keeping them is what stops
      ;; every further keystroke re-asking the server about the same position.
      (t (completion-show))))
  nil)

(defun %lsp-completion-filter (prefix)
  "Narrow the candidates we already have to PREFIX and redraw."
  (let ((rows (loop for c in (getf *lsp-completion* :items)
                    when (%lsp-prefix-p prefix (second c)) collect c)))
    (setf (getf *lsp-completion* :shown)
          (subseq rows 0 (min (length rows) *lsp-completion-limit*))
          (getf *lsp-completion* :index) 0)
    (%lsp-completion-draw)))

(defun %lsp-completion-reply (path at result error)
  "A server's answer, checked against the buffer as it is *now*.

Four ways an answer is stale and all four are the same answer — drop it: the
request was superseded, you are in another file, the word moved, or you stopped
typing a word at all. Nothing is retried, because the next keystroke asks again."
  (let ((live *lsp-completion*))
    (when (and live
               (null error)
               (eql at (getf live :at))
               (equal path (buffer-file-name)))
      (setf (getf *lsp-completion* :items)
            (loop for item in (%lsp-completion-items result)
                  for row = (%lsp-completion-row item)
                  when row collect row))
      (let ((where (%lsp-completion-prefix)))
        (if (and where (eql (car where) at))
            (%lsp-completion-filter (cdr where))
            (%lsp-completion-hide))))))

(defun %lsp-completion-request (at)
  "Ask the server what can follow the word starting at AT.

The old popup is replaced *before* the request goes out rather than left up
until the reply, so the anchor recorded here is what the reply is checked
against. A popup showing the previous word's candidates while the next word's
are in flight is the one stale state worth spending a frame of emptiness on."
  (let ((key (lsp-session-for-buffer))
        (path (buffer-file-name)))
    (cond
      ;; No server, or one that declined to complete. The old popup still has to
      ;; come down: point has moved to a word this list is not about, and
      ;; leaving it up would be the one thing worse than showing nothing.
      ((not (and key path (%lsp-get key :completion))) (%lsp-completion-hide))
      (t
       (setf *lsp-completion*
             (list :at at :path path :items nil :shown nil :index 0))
       (rpc-request
        (%lsp-get key :conn) "textDocument/completion"
        (jobj "textDocument" (jobj "uri" (lsp-path-uri path))
              ;; LSP counts lines from 0 and `line-number' from 1. The column is
              ;; the same approximation `lsp-goto-definition' makes and is
              ;; written up on `%lsp-capabilities': characters where LSP wants
              ;; UTF-16 units, exact on ASCII.
              "position" (jobj "line" (1- (line-number)) "character" (column))
              ;; 1 is `Invoked' — this client has no trigger characters, since
              ;; deciding to complete is a rule in this file rather than a
              ;; property of the server.
              "context" (jobj "triggerKind" 1))
        (lambda (result error) (%lsp-completion-reply path at result error)))
       nil))))

(defun lsp-complete-maybe (&optional force)
  "Keep the popup in step with the word point is in. On `*after-change-functions*'.

Cheap on the path that matters, which is a keystroke in a buffer with no server:
one string compare on the mode, then one hash lookup, and out.

Re-asks the server only when the word *starts* somewhere new. Typing further
into a word we already have candidates for narrows them locally, which is what
makes the popup feel instant — and is legitimate, because the server's list for
`fo' contains its list for `foo'. ponytail: `isIncomplete' is ignored, so a
server that truncated its answer is not asked again and the list can be short by
whatever it left out. Ceiling: a huge global namespace two characters in.
Upgrade path is one `jget' and a flag on the plist that forces the re-ask."
  (when (string= (evil-state) "insert")
    (let ((where (and (lsp-session-for-buffer) (%lsp-completion-prefix))))
      (cond
        ((null where) (%lsp-completion-hide))
        ((and (not force) (< (length (cdr where)) *lsp-completion-minimum*))
         (%lsp-completion-hide))
        ((and *lsp-completion*
              (not force)
              (eql (car where) (getf *lsp-completion* :at))
              (equal (buffer-file-name) (getf *lsp-completion* :path)))
         (%lsp-completion-filter (cdr where)))
        (t (%lsp-completion-request (car where))))))
  nil)

(pushnew 'lsp-complete-maybe *after-change-functions*)

;;; ---------------------------------------------------------------------------
;;; The keys
;;;
;;; `C-n' `C-p' `C-y' `C-e', which is vim's insert-mode completion and not
;;; corfu's `RET'/`TAB' — and the choice is about the async gap rather than about
;;; taste. Every one of these is a *dead key in Insert mode today*: core answers
;;; a bare `Ctrl' with no command at all. So binding them costs nothing and needs
;;; no fallback.
;;;
;;; `RET' and `TAB' would need one, and the fallback is what makes them wrong: a
;;; Lisp command that inserted a newline when no popup was up would insert it a
;;; queue turn later, *after* whatever you typed next — so `a<RET>b' would come
;;; out as `ab' and a newline. There is no way to write that command correctly
;;; from the image, which is exactly the sort of thing `docs/threading.org'
;;; exists to say out loud.

(defun %lsp-completion-move (delta)
  "Move the selection by DELTA, wrapping.

One `completion-show' and no rows: the candidates have not changed, and
resending them would be one command per candidate on a keypress whose whole
point is to be cheap."
  (let ((rows (and *lsp-completion* (getf *lsp-completion* :shown))))
    (when rows
      (let ((i (mod (+ (getf *lsp-completion* :index) delta) (length rows))))
        (setf (getf *lsp-completion* :index) i)
        (completion-show (getf *lsp-completion* :at) i))))
  nil)

(defun lsp-complete-next ()
  "Highlight the next candidate. Nothing when no popup is up."
  (%lsp-completion-move 1))

(defun lsp-complete-previous ()
  "Highlight the previous candidate."
  (%lsp-completion-move -1))

(defun lsp-complete-abort ()
  "Dismiss the popup, leaving what you typed alone."
  (%lsp-completion-hide))

(defun lsp-complete-accept ()
  "Replace the word being typed with the highlighted candidate.

The anchor comes from the *editor* — `(completion-at)' — rather than from our own
plist, because core hides a popup point has walked out of without saying so, and
completing a word whose box is not on screen is the one failure this command can
have. NIL means there is nothing to accept."
  (let ((at (completion-at))
        (row (and *lsp-completion*
                  (nth (getf *lsp-completion* :index)
                       (getf *lsp-completion* :shown)))))
    (cond ((or (null at) (null row)) (%lsp-completion-hide))
          (t
           ;; One primitive over the whole range: `docs/threading.org'. A
           ;; `delete-region' and an `insert' would be two undo steps with a
           ;; keystroke's width between them.
           ;;
           ;; ponytail: `at' and `(point)' are two reads, so a keystroke landing
           ;; between them widens the range by a character. Same window every
           ;; two-reader command in this editor has; closing it means the range
           ;; travelling with the command.
           (replace-region at (point) (first row))
           (%lsp-completion-hide))))
  nil)

(defun lsp-complete ()
  "Ask for completions here and now, however little of the word is typed."
  (cond ((null (lsp-session-for-buffer)) (message "lsp: no server for this buffer"))
        ((not (string= (evil-state) "insert")) (message "lsp: completion is an Insert-mode thing"))
        (t (lsp-complete-maybe t)))
  nil)

;;; ponytail: `hover' and `signatureHelp' are the obvious next two and are
;;; deliberately not here. Both are the same round trip with a different method
;;; name, and both want a surface this editor does not have — a *transient
;;; paragraph* near point, which is neither the one-line echo area nor a box you
;;; navigate. Building them on `completion-show' would give you a popup whose
;;; rows are wrapped prose, which is the wrong shape twice.

;;; ---------------------------------------------------------------------------
;;; What ships
;;;
;;; Two servers, because these are the two the client was proved against. A
;;; third is one line, and belongs in your config rather than here.

(lsp-register-server 'python-mode "pylsp")
(lsp-register-server 'c-mode "clangd" "--background-index")

;;; Readers and internals are not commands: running `lsp-diagnostics' by hand
;;; builds a list nobody sees. `*hidden-commands*' is the init file's lever, and
;;; is only bound when this file is loaded from one.
(when (boundp '*hidden-commands*)
  (dolist (n '("lsp-diagnostics" "lsp-ensure" "lsp-did-save" "after-change-hook"
               "lsp-session-for-buffer"
               ;; The completion keys. `M-x lsp-complete-next' with no popup up
               ;; does nothing at all, which is not a command — it is half of a
               ;; key binding, and the other half is the popup.
               "lsp-complete-maybe" "lsp-complete-next" "lsp-complete-previous"
               "lsp-complete-accept" "lsp-complete-abort"))
    (pushnew n (symbol-value '*hidden-commands*) :test #'string=)))

(export '(lsp lsp-stop lsp-restart lsp-status lsp-goto-definition
          lsp-diagnostics lsp-diagnostics-at-point lsp-list-diagnostics
          lsp-register-server lsp-project-root lsp-path-uri lsp-uri-path
          lsp-severity-name *lsp-diagnostics-functions* *lsp-servers*
          *after-change-functions*
          lsp-complete lsp-complete-next lsp-complete-previous
          lsp-complete-accept lsp-complete-abort *lsp-completion-minimum*
          *lsp-completion-limit*)
        :zemacs)
