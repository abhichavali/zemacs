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
;;;;
;;;; ---------------------------------------------------------------------------
;;;; What the editor tells us, and what it costs
;;;;
;;;; One signal: `after-change-hook', which the application fires whenever the
;;;; document's revision moves. It carries no *delta* — core keeps no edit log
;;;; Lisp can read — so `textDocument/didChange' sends the **whole buffer** every
;;;; time.
;;;;
;;;; ponytail: full text rather than incremental sync, and the reason is not
;;;; laziness about the diffing. Incremental sync needs (start, old-end, new-end)
;;;; per edit, and there is nowhere to get it: the only change notification that
;;;; exists is "the revision moved". Building one is a line in core — the same
;;;; after-change record overlays will want — and it should be built once, for
;;;; both, rather than approximated here by diffing two copies of a buffer on
;;;; every keystroke. Until then this costs one buffer copy per revision, which
;;;; is what Emacs paid for years and is not felt below a few hundred KB.
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
                     (declare (ignore result))
                     (%lsp-initialized key error)))
      key)))

(defun %lsp-initialized (key error)
  "The reply to `initialize'. Nothing may be sent to a server before
`initialized' goes out, which is why everything until now was queued."
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
;;; Until that exists the presentation below is the whole of it: a summary in
;;; the echo area when the diagnostics for the buffer you are looking at change,
;;; a command that reads out the one on the current line, and a listing buffer.
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

(defparameter *lsp-diagnostics-file*
  (merge-pathnames ".config/zemacs/diagnostics.txt" (user-homedir-pathname))
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
               "lsp-session-for-buffer"))
    (pushnew n (symbol-value '*hidden-commands*) :test #'string=)))

(export '(lsp lsp-stop lsp-restart lsp-status lsp-goto-definition
          lsp-diagnostics lsp-diagnostics-at-point lsp-list-diagnostics
          lsp-register-server lsp-project-root lsp-path-uri lsp-uri-path
          lsp-severity-name *lsp-diagnostics-functions* *lsp-servers*
          *after-change-functions*)
        :zemacs)
