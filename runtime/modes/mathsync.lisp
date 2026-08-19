;;;; mathsync — a photograph of a blackboard becomes org LaTeX you can paste.
;;;;
;;;; The workflow this serves already existed on the disk before any of this
;;;; code did: images arrive in `~/Public/MathSync' from a phone, and the ones
;;;; that have been dealt with sit in `done/' beside them. So the *directory is
;;;; the queue*, and this file adds no database, no index and no state file —
;;;; "what is waiting" is answered by listing a folder, and "this one is dealt
;;;; with" is said by moving a file. Anything else would be a second opinion
;;;; about the same question, and the two would disagree the first time a file
;;;; was moved by hand.
;;;;
;;;; Three things happen here and only the middle one is interesting:
;;;;
;;;;   watching     — a scan of one directory, throttled to a few seconds, on
;;;;                  `*point-moved-functions*'. There is no timer in this
;;;;                  editor; `math-code.lisp' and `project.lisp' poll the same
;;;;                  way and the head of the former explains why at length.
;;;;   transcribing — `runtime/mathsync_transcribe.py', run detached. It is in
;;;;                  Python and not here because the ECL image has no HTTP
;;;;                  client and no JSON *reader* — `rpc.lisp' encodes, and the
;;;;                  decoding half of that pair lives in Rust for the LSP — so
;;;;                  doing it in Lisp means writing TLS, base64 and a parser
;;;;                  first. Python's standard library has all three.
;;;;   answering    — `set-register', so the result lands in the register `p'
;;;;                  pastes from. Not `insert': the transcription finishes
;;;;                  while you are somewhere else entirely, and an editor that
;;;;                  typed a screenful of LaTeX into whatever buffer happened
;;;;                  to be in front of you would be a bug with a feature's
;;;;                  name. The register waits until you ask.
;;;;
;;;; Nothing here blocks. The child is started with `:wait nil' and redirects
;;;; its own output to files, so no pipe can fill and nothing drains one — the
;;;; arrangement `math-code.lisp' arrived at, and for its reason: this runs on
;;;; the thread that also runs every keystroke's `after-change-hook'.

(in-package :zemacs)

(defparameter *mathsync-dir* "~/Public/MathSync/"
  "The folder watched for new images. `~/' is expanded.")

(defparameter *mathsync-keep* nil
  "Keep a transcribed image, in `*mathsync-done-name*', instead of deleting it.

Deleting by default, which is a reversal and worth the note. The argument for
moving was that the model gets things wrong and the photograph is the only copy
of what it was reading — true, and it loses to what the folder actually is: a
phone drops a picture in, you paste the LaTeX, and the picture has done its job.
A `done/' that grows for ever is a second folder to clean out by hand, which is
exactly the bookkeeping this file was written to avoid having.

Either way the image leaves the queue. It has to: the directory *is* the queue,
so an image that stayed put would be offered again on the next scan for ever and
cost a second paid API call.

Set this to T if you would rather keep them.")

(defparameter *mathsync-done-name* "done/"
  "Where a transcribed image is moved when `*mathsync-keep*' is on, relative to
`*mathsync-dir*'.")

(defparameter *mathsync-model* "google/gemini-3.7-flash"
  "The OpenRouter model asked to read the image. Must be able to see one.")

(defparameter *mathsync-key-file* "~/.zemacs.d/openrouter-key"
  "A file holding nothing but an OpenRouter API key.

A file and not `$OPENROUTER_API_KEY' because this editor is a GUI application:
launched from Finder it inherits no shell, so an export in a shell profile is
not there when the key is wanted. Outside the repository by construction, and
worth `chmod 600'.")

(defparameter *mathsync-types* '("png" "jpg" "jpeg" "webp" "gif")
  "Extensions treated as images to transcribe, lower-cased.")

(defparameter *mathsync-scan-interval* 3
  "Seconds between directory scans.

The poll below runs on every cursor movement, and a `directory' call per
keystroke is a filesystem round trip per keystroke. Three seconds is far below
the rate at which a photograph arrives from a phone and far above the rate at
which anyone types.")

(defvar *mathsync-job* nil
  "The transcription in flight — a plist of :process and :image — or NIL.
NIL is the whole of \"nothing is running\": there is no second flag.")

(defvar *mathsync-waiting* 0
  "Images seen waiting at the last scan. The number on the modeline.")

(defvar *mathsync-ready* nil
  "True when a transcription is sitting in the register unpasted.

Cleared by starting the next one rather than by pasting: nothing tells this file
that a register was read, and a tick that lingered one job too long is a better
lie than one that never appears.")

(defvar *mathsync-last-scan* 0
  "`get-universal-time' at the last directory scan.")

;;; ---------------------------------------------------------------------------
;;; The queue, which is a directory

(defun %mathsync-dir ()
  "The watch folder, always as a *directory* pathname.

The trailing slash is forced rather than assumed, and it is not a nicety:
`merge-pathnames' reads the last component of a slashless path as a *file name*,
so `~/Public/MathSync' — which is how anybody would write it, and how anybody
will write it when they change this setting — would look for images in `~/Public'
beside the folder rather than inside it, and find none for ever."
  (let ((path (%expand-home *mathsync-dir*)))
    (if (and (plusp (length path))
             (char= (char path (1- (length path))) #\/))
        path
        (concatenate 'string path "/"))))

(defun %mathsync-done ()
  (merge-pathnames *mathsync-done-name* (%mathsync-dir)))

(defun %mathsync-image-p (path)
  "True when PATH is a file this would send to the model.

`pathname-name' is the test that excludes `done/' without naming it: a directory
pathname has none, so subfolders drop out here rather than in a second check
that would have to know what they are called."
  (and (pathname-name path)
       (member (string-downcase (or (pathname-type path) ""))
               *mathsync-types* :test #'string=)))

(defun %mathsync-images ()
  "Every image waiting in the watch folder, oldest first.

Oldest first so a backlog is worked through in the order it arrived, which is
the order the blackboard was photographed in and therefore the order the
transcriptions make sense in."
  (let ((dir (%mathsync-dir)))
    (when (probe-file dir)
      (sort (remove-if-not #'%mathsync-image-p
                           (ignore-errors (directory (merge-pathnames "*.*" dir))))
            #'<
            :key (lambda (p) (or (ignore-errors (file-write-date p)) 0))))))

;;; ---------------------------------------------------------------------------
;;; The modeline note

(defun %mathsync-note ()
  "Put the watcher's state on the modeline, or take it down when there is none.

Four states and one line. Empty is the ordinary one, and an empty `%N' drops its
whole segment — so a folder with nothing in it costs no room on the strip."
  (modeline-note
   (cond
     (*mathsync-job* "MathSync ⋯")
     ((and *mathsync-ready* (plusp *mathsync-waiting*))
      (format nil "MathSync ✓ ~d" *mathsync-waiting*))
     (*mathsync-ready* "MathSync ✓")
     ((plusp *mathsync-waiting*) (format nil "MathSync ~d" *mathsync-waiting*))
     (t nil)))
  nil)

;;; ---------------------------------------------------------------------------
;;; Running the transcriber

(defun %mathsync-tmp (suffix)
  (merge-pathnames (format nil "zemacs-mathsync.~a" suffix) #p"/tmp/"))

(defun %mathsync-quote (thing)
  "THING as one single-quoted word for `/bin/sh'.

Not decoration. A macOS screenshot is called `Screenshot 2026-06-09 at 9.31.55 PM.png'
— spaces, and a narrow no-break space before the `PM' — so an unquoted path is a
command with nine arguments in it. The `'\\''' dance closes the quote, escapes a
literal one, and opens it again, which is the only way to put a quote inside a
single-quoted word."
  (let ((text (if (stringp thing) thing (namestring thing))))
    (with-output-to-string (s)
      (write-char #\' s)
      (loop for c across text
            do (if (char= c #\')
                   (write-string "'\\''" s)
                   (write-char c s)))
      (write-char #\' s))))

(defun %mathsync-read (path)
  "PATH's whole contents as a string, or NIL when it is not there."
  (when (probe-file path)
    (ignore-errors
     (with-open-file (in path :external-format :utf-8)
       (let ((text (make-string (file-length in))))
         (subseq text 0 (read-sequence text in)))))))

(defun %mathsync-first-line (text)
  (let ((end (position #\Newline text)))
    (string-trim '(#\Space #\Tab #\Return) (subseq text 0 (or end (length text))))))

(defun %mathsync-start (image)
  "Start transcribing IMAGE, detached.

A shell script rather than `python3' directly, for the reason `math-code.lisp'
writes one: the child redirects its *own* output to files on its first line, so
there is no pipe for this thread to drain and therefore no full pipe to deadlock
on. What comes back is read off the disk when the process is seen to have gone."
  (let ((script (runtime-file "mathsync_transcribe.py"))
        (key (%expand-home *mathsync-key-file*)))
    (cond
      ((not (and script (probe-file script)))
       (message "mathsync: mathsync_transcribe.py is not beside the runtime"))
      ((not (probe-file key))
       (message (format nil "mathsync: no API key at ~a" key)))
      (t
       (handler-case
           (let ((sh (%mathsync-tmp "sh")))
             (with-open-file (s sh :direction :output :if-exists :supersede
                                   :if-does-not-exist :create)
               (format s "#!/bin/sh~%exec python3 ~a ~a ~a ~a >~a 2>~a~%"
                       (%mathsync-quote script)
                       (%mathsync-quote image)
                       (%mathsync-quote *mathsync-model*)
                       (%mathsync-quote key)
                       (%mathsync-quote (%mathsync-tmp "out"))
                       (%mathsync-quote (%mathsync-tmp "err"))))
             (multiple-value-bind (stream code process)
                 (ext:run-program "/bin/sh" (list (namestring sh))
                                  :input nil :output nil :error nil :wait nil)
               (declare (ignore stream code))
               (setf *mathsync-job* (list :process process :image image)
                     *mathsync-ready* nil)
               (%mathsync-note)
               (message (format nil "mathsync: reading ~a…"
                                (file-namestring image)))))
         (serious-condition (e)
           (message (format nil "mathsync: cannot start the transcriber — ~a" e))))))))

(defun %mathsync-archive (image)
  "Take IMAGE out of the queue: delete it, or move it into `done/'.

Failing either way is reported and not silent: the directory is the queue, so an
image left in place will be offered again on the next scan and transcribed twice,
which costs a second API call and looks like the editor ignoring you.

The two branches share this function rather than being chosen at the call site,
because what the caller means is `this one is dealt with' and *how* is the
policy — see `*mathsync-keep*'."
  (handler-case
      (if *mathsync-keep*
          (let ((done (%mathsync-done)))
            (ensure-directories-exist done)
            (rename-file image (merge-pathnames (file-namestring image) done)))
          (delete-file image))
    (serious-condition (e)
      (message (format nil "mathsync: transcribed, but ~a stayed put — ~a"
                       (file-namestring image) e)))))

(defun %mathsync-finish ()
  "The child has gone: take what it wrote and put it in the register."
  (let* ((image (getf *mathsync-job* :image))
         (out (%mathsync-read (%mathsync-tmp "out")))
         (err (%mathsync-read (%mathsync-tmp "err")))
         (text (and out (string-trim '(#\Space #\Tab #\Newline #\Return) out))))
    (setf *mathsync-job* nil)
    (cond
      ((and text (plusp (length text)))
       ;; The unnamed register, which is what `p' pastes — see `set-register'.
       (set-register text)
       (setf *mathsync-ready* t)
       (%mathsync-archive image)
       (message (format nil "mathsync: ~a transcribed — `p' to paste"
                        (file-namestring image))))
      (t
       ;; The script puts one line on stderr for every failure it has, and that
       ;; line is written to be read here: a 404 on the model slug says so.
       (message (format nil "mathsync: ~a"
                        (if (and err (plusp (length (%mathsync-first-line err))))
                            (%mathsync-first-line err)
                            "the transcriber said nothing")))))
    (%mathsync-note)))

;;; ---------------------------------------------------------------------------
;;; The poll, and the command

(defun mathsync-poll ()
  "Notice that a transcription finished, and that images have arrived.

On `*point-moved-functions*'. Costs one special-variable read per movement, and
one more plus a `minor-mode-p' in an org buffer.

**The two halves are gated differently, and that asymmetry is the whole of this
function.** `mathsync-transcribe' is on `SPC n m', which `define-leader' binds
in every buffer there is — deliberately, because the folder is watched whatever
you are looking at. So a job can be started from anywhere, and gating the
*finish* on the mode meant a transcription begun in an org buffer and waited for
anywhere else — a `.py' tangled out of the curriculum, which is exactly where
you go while it runs — never landed in the register at all. `*mathsync-job*'
stayed set for the rest of the session, so every later `SPC n m' answered `still
reading' about a child that had exited minutes ago.

The scan is the half that costs something — a `directory' call — and it is the
half that is only interesting where the note is drawn, so it keeps the mode
test. Finishing a job costs a process-handle read and only when there is a job,
which is almost never."
  ;; The job first: it is the answer somebody is waiting for, and it is owed
  ;; wherever they happen to be standing when it arrives.
  (when *mathsync-job*
    (let ((process (getf *mathsync-job* :process)))
      (when (handler-case
                (not (eq :running (ext:external-process-wait process nil)))
              ;; A handle that can no longer be asked is a job that can no
              ;; longer be followed; the output files are the real verdict.
              (serious-condition () t))
        (%mathsync-finish))))
  ;; ...and then the folder, at the throttled rate, where the note is shown.
  (when (minor-mode-p 'mathsync)
    (let ((now (get-universal-time)))
      (when (>= (- now *mathsync-last-scan*) *mathsync-scan-interval*)
        (setf *mathsync-last-scan* now)
        (let ((n (length (%mathsync-images))))
          (unless (eql n *mathsync-waiting*)
            (setf *mathsync-waiting* n)
            (%mathsync-note))))))
  nil)

(defun mathsync-transcribe ()
  "Transcribe the oldest image waiting in `*mathsync-dir*'.

One at a time and by hand, which is the whole of the policy: each call is a paid
request against somebody's API key, and a watcher that spent one by itself every
time a file appeared would be a bill you did not agree to. The modeline says how
many are waiting; this is how you spend one."
  (cond
    (*mathsync-job*
     (message (format nil "mathsync: still reading ~a"
                      (file-namestring (getf *mathsync-job* :image)))))
    (t
     (let ((waiting (%mathsync-images)))
       (setf *mathsync-waiting* (length waiting)
             *mathsync-last-scan* (get-universal-time))
       (if (null waiting)
           (progn (%mathsync-note)
                  (message (format nil "mathsync: nothing waiting in ~a"
                                   (%mathsync-dir))))
           (%mathsync-start (first waiting))))))
  nil)

(define-minor-mode mathsync
  "Watch a folder for photographs of mathematics and transcribe them to org LaTeX.

On in org buffers by default. `SPC n m' transcribes the oldest image waiting;
the result goes in the register, so `p' pastes it. The modeline says how many
are waiting, `⋯' while one is being read, and `✓' when one is in the register."
  (:on (setf *mathsync-last-scan* 0) (%mathsync-note))
  ;; The note is about a folder rather than about this buffer, but switching the
  ;; mode off is the one unambiguous statement that you are not interested.
  (:off (modeline-note nil)))

(defun mathsync-enable ()
  "Switch the mode on for an org buffer, once. On `*org-mode-functions*'."
  (enable-minor-mode 'mathsync)
  nil)

(defun %mathsync-note-click (text)
  "What the `MathSync' note on the modeline means, when it is clicked.

On `*modeline-note-functions*'. The note is four glyphs wide because the strip
is; this is where the sentence goes. Answers NIL for anybody else's note, which
is what lets several modes share the one segment."
  (when (and (>= (length text) 8) (string= "MathSync" text :end2 8))
    (cond
      (*mathsync-job*
       (format nil "MathSync: reading ~a…"
               (file-namestring (getf *mathsync-job* :image))))
      (*mathsync-ready*
       (if (plusp *mathsync-waiting*)
           (format nil "MathSync: a transcription is in the register — `p' pastes it; ~d more waiting"
                   *mathsync-waiting*)
           "MathSync: a transcription is in the register — `p' pastes it"))
      ((plusp *mathsync-waiting*)
       (format nil "MathSync: ~d image~:p waiting in ~a — `SPC n m' reads the oldest"
               *mathsync-waiting* (%mathsync-dir)))
      (t (format nil "MathSync: nothing waiting in ~a" (%mathsync-dir))))))

;;; The key, if nothing else claimed it.
;;;
;;; A mode binding its own leader key is unusual here and this one has earned
;;; it: `SPC n m' lived in `runtime/init.lisp', which is only the *shipped*
;;; config — the moment you have a `~/.zemacs.d/init.lisp' that file is what
;;; runs, and one written before this feature existed has no `SPC n' group in
;;; it at all. So the mode loaded, the folder was watched, the note appeared on
;;; the strip, and there was no way to press the key it names. `default-modeline'
;;; is in `library.lisp' for exactly this reason.
;;;
;;; Guarded twice. `fboundp' because a mode file must load whether or not the
;;; init that loaded it has defined `define-leader' yet; `leader-key' because a
;;; config that put this somewhere else has already spoken and must win.
(when (and (fboundp 'define-leader)
           (fboundp 'leader-key)
           (not (leader-key "mathsync-transcribe")))
  (define-leader "SPC n m" "mathsync-transcribe"))

(add-hook '*org-mode-functions* 'mathsync-enable)
(add-hook '*point-moved-functions* 'mathsync-poll)
(add-hook '*modeline-note-functions* '%mathsync-note-click)
