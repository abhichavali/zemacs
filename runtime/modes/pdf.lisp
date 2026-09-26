;;;; pdf.lisp — reading a PDF, a page at a time.
;;;;
;;;; `mutool' (MuPDF's command-line tool, `brew install mupdf-tools') draws a
;;;; page into a PNG exactly as wide as the window, `image-file' loads it, and a
;;;; scene shows it: a page in pixels, scrolled in pixels. What Rust added for it
;;;; is three readers in `gui.lisp' — `window-pixel-size', `scene-scroll' and
;;;; `image-size' — and nothing else.
;;;;
;;;; The application opens a file that is not text as a read-only buffer holding
;;;; its name, the way it opens a picture, and `pdf-mode' is picked by extension.
;;;; So dired's RET, `find-file', recent files and the project finder all land
;;;; here without knowing it.
;;;;
;;;; Pages are drawn once per size and kept in `$TMPDIR/zemacs-pdf', so turning
;;;; back is a file read and turning forward is ~70 ms of mutool. The name of a
;;;; drawn page carries the PDF's write date, so a document LaTeX has just
;;;; rebuilt is drawn afresh by `g r' rather than shown stale.
;;;;
;;;; ponytail: one page at a time rather than a continuous strip — `j' at the
;;;; bottom of a page turns it, which reads the same. No horizontal panning,
;;;; since a scene scrolls on one axis, so zoom tops out at the window's width.
;;;; Search finds the page, not the word on it; `mutool draw -F stext' has the
;;;; boxes when that is missed.

(in-package :zemacs)

(defparameter *pdf-program* "mutool"
  "MuPDF's command-line tool, which draws the pages and extracts the text.")

(defparameter *pdf-pad* 16
  "Margin around the page, in device pixels.")

(defparameter *pdf-scroll-step* 0.08
  "How far `j' and `k' move, as a fraction of the window's height.")

(defvar *pdf-documents* (make-hash-table :test #'equal)
  "File -> its reading state, a plist. DEFVAR so reloading the config keeps
your place in every PDF you have open, and so does killing and reopening one.")

(defvar *pdf-last-search* nil
  "What `/' last looked for, for `n' and `N'.")

(defun %pdf-doc ()
  "The live buffer's reading state, or NIL (and a message) when it is not a PDF."
  (let* ((file (buffer-file-name))
         (doc (and file (gethash file *pdf-documents*))))
    (unless (getf doc :count)
      (message "not a PDF this editor could read"))
    (and (getf doc :count) doc)))

(defun %pdf-complaint (out)
  "The first line of what mutool said, for a status line that has one."
  (first (split-string (string-trim '(#\Space #\Newline) (or out "")) #\Newline)))

(defun %pdf-cache-dir ()
  (ensure-directories-exist
   (format nil "~a/zemacs-pdf/"
           (string-right-trim "/" (or (ext:getenv "TMPDIR") "/tmp")))))

(defun %pdf-load (doc file)
  "Count FILE's pages into DOC, unless DOC already describes this copy of it."
  (let ((stamp (ignore-errors (file-write-date file))))
    (unless (and stamp (eql stamp (getf doc :stamp)))
      (multiple-value-bind (out status)
          (run-process *pdf-program* (list "show" file "trailer/Root/Pages/Count"))
        ;; The last line, because stderr is merged in and a damaged file gets a
        ;; warning or two printed ahead of the answer.
        (let ((n (and (eq status :exited)
                      (some (lambda (l) (parse-integer l :junk-allowed t))
                            (reverse (split-string out #\Newline))))))
          (setf (getf doc :stamp) stamp
                (getf doc :count) (and n (plusp n) n)
                (getf doc :text) nil
                (getf doc :page) (max 1 (min (getf doc :page) (or n 1))))
          (unless (getf doc :count)
            (message (format nil "pdf: cannot read ~a — ~a" file (%pdf-complaint out)))))))))

(defun %pdf-png (doc file w h)
  "The PNG of DOC's page drawn to fit W by H pixels (H NIL for any height),
drawing it first if it is not on disk. NIL, having said why, when mutool fails."
  (let* ((page (getf doc :page))
         (png (format nil "~a~36r-~d-~dx~d~:[~;-i~].png" (%pdf-cache-dir)
                      (sxhash (format nil "~a ~a" file (getf doc :stamp)))
                      page w (or h 0) (getf doc :invert))))
    (or (probe-file png)
        (multiple-value-bind (out status)
            (run-process *pdf-program*
                         `("draw" "-q" "-o" ,png "-w" ,(princ-to-string w)
                           ,@(when h (list "-h" (princ-to-string h)))
                           ,@(when (getf doc :invert) '("-I"))
                           ,file ,(princ-to-string page)))
          (if (and (eq status :exited) (probe-file png))
              png
              (progn
                ;; A timeout can leave half a file, which would otherwise be
                ;; found and trusted next time.
                (ignore-errors (delete-file png))
                (message (format nil "pdf: page ~d — ~a" page (%pdf-complaint out)))
                nil))))))

(defun %pdf-show (doc &optional scroll)
  "Put DOC's page up, fitted to the window. SCROLL is pixels from the top,
`:bottom', or NIL to keep the reader where they were."
  (destructuring-bind (&optional (vw 0) (vh 0)) (window-pixel-size)
    ;; Zero before the first frame has been drawn, which nothing should see.
    (let* ((vw (if (plusp vw) vw 1000))
           (vh (if (plusp vh) vh 800))
           (zoom (getf doc :zoom))
           (w (max 64 (round (* zoom (- vw (* 2 *pdf-pad*))))))
           (h (when (eq (getf doc :fit) :page)
                (max 64 (round (* zoom (- vh (* 2 *pdf-pad*)))))))
           (png (%pdf-png doc (buffer-file-name) w h))
           (id (and png (image-file png))))
      (when id
        (scene-set (block :pad *pdf-pad* :width (pct 100) :align :center (image id)))
        (let ((bottom (max 0 (- (+ (second (image-size id)) (* 2 *pdf-pad*)) vh))))
          (setf (getf doc :bottom) bottom)
          (case scroll
            ((nil))
            (:bottom (scene-scroll bottom))
            (t (scene-scroll scroll))))
        (message (format nil "page ~d of ~d~:[~;  (~d%)~]" (getf doc :page)
                         (getf doc :count) (/= zoom 1) (round (* 100 zoom))))))))

(defun %pdf-goto (page &optional (scroll 0))
  (let ((doc (%pdf-doc)))
    (when doc
      (setf (getf doc :page) (max 1 (min page (getf doc :count))))
      (%pdf-show doc scroll)))
  nil)

(define-derived-mode pdf-mode nil
  (let ((file (buffer-file-name)))
    (cond
      ((null file) (message "pdf-mode shows a PDF file, and this buffer has none"))
      ((not (executable-find *pdf-program*))
       (message "pdf-mode draws pages with mutool — brew install mupdf-tools"))
      ;; A binary older than this file answers NIL to a reader it has never
      ;; heard of — see `check-required-primitives' for why this says so.
      ((null (window-pixel-size))
       (message "zemacs is older than pdf-mode — run scripts/install.sh, then restart"))
      (t
        ;; Every key is here from the start, so `setf getf' below always finds
        ;; one to change rather than consing a new head nobody holds.
        (let ((doc (or (gethash file *pdf-documents*)
                       (setf (gethash file *pdf-documents*)
                             (list :page 1 :count nil :stamp nil :text nil
                                   :zoom 1 :fit :width :invert nil :bottom 0)))))
          (%pdf-load doc file)
          (when (getf doc :count)
            (%pdf-show doc 0)))))))

(add-auto-mode ".pdf" 'pdf-mode)

;;; ---------------------------------------------------------------------------
;;; Moving

(defun %pdf-scroll (fraction)
  "Scroll FRACTION of the window's height, turning the page at either end."
  (let ((doc (%pdf-doc)))
    (when doc
      (let ((y (or (scene-scroll) 0))
            (dy (round (* fraction (or (second (window-pixel-size)) 800))))
            (page (getf doc :page))
            (bottom (getf doc :bottom)))
        (cond ((and (plusp dy) (>= y bottom) (< page (getf doc :count)))
               (%pdf-goto (1+ page) 0))
              ((and (minusp dy) (<= y 0) (> page 1))
               (%pdf-goto (1- page) :bottom))
              (t (scene-scroll (max 0 (min bottom (+ y dy)))))))))
  nil)

(defun pdf-scroll-down ()
  "Scroll down a little, or on to the next page from the bottom of this one."
  (%pdf-scroll *pdf-scroll-step*))

(defun pdf-scroll-up ()
  "Scroll up a little, or back to the previous page from the top of this one."
  (%pdf-scroll (- *pdf-scroll-step*)))

(defun pdf-scroll-down-half ()
  "Scroll down half a window."
  (%pdf-scroll 1/2))

(defun pdf-scroll-up-half ()
  "Scroll up half a window."
  (%pdf-scroll -1/2))

(defun pdf-scroll-down-window ()
  "Scroll down a window, less a sliver to keep your place by."
  (%pdf-scroll 9/10))

(defun pdf-scroll-up-window ()
  "Scroll up a window, less a sliver to keep your place by."
  (%pdf-scroll -9/10))

(defun pdf-next-page ()
  "Turn to the next page."
  (let ((doc (%pdf-doc))) (when doc (%pdf-goto (1+ (getf doc :page))))))

(defun pdf-previous-page ()
  "Turn back to the previous page."
  (let ((doc (%pdf-doc))) (when doc (%pdf-goto (1- (getf doc :page))))))

(defun pdf-first-page ()
  "Go to the first page."
  (%pdf-goto 1))

(defun pdf-last-page ()
  "Go to the last page."
  (let ((doc (%pdf-doc))) (when doc (%pdf-goto (getf doc :count)))))

(defun pdf-goto-page ()
  "Ask for a page number and go there."
  (when (%pdf-doc)
    (read-string "Page: "
      (lambda (answer)
        (let ((n (and answer (parse-integer answer :junk-allowed t))))
          (when n (%pdf-goto n))))))
  nil)

;;; ---------------------------------------------------------------------------
;;; Size and colour

(defun %pdf-redraw (&rest changes)
  "Apply CHANGES, a plist, to the reading state and draw the page again."
  (let ((doc (%pdf-doc)))
    (when doc
      (loop for (k v) on changes by #'cddr do (setf (getf doc k) v))
      (%pdf-show doc)))
  nil)

(defun pdf-zoom-in ()
  "Draw the page larger, up to the fit it is in."
  (let ((doc (%pdf-doc)))
    (when doc
      (if (>= (getf doc :zoom) 1)
          (message "already as large as the window — no horizontal scrolling here")
          (%pdf-redraw :zoom (min 1 (* (getf doc :zoom) 5/4)))))))

(defun pdf-zoom-out ()
  "Draw the page smaller."
  (let ((doc (%pdf-doc)))
    (when doc (%pdf-redraw :zoom (max 1/5 (* (getf doc :zoom) 4/5))))))

(defun pdf-fit-width ()
  "Draw the page as wide as the window. Also re-fits after a resize."
  (%pdf-redraw :fit :width :zoom 1))

(defun pdf-fit-page ()
  "Draw the whole page inside the window."
  (%pdf-redraw :fit :page :zoom 1))

(defun pdf-midnight ()
  "Toggle light text on a dark page."
  (let ((doc (%pdf-doc)))
    (when doc (%pdf-redraw :invert (not (getf doc :invert))))))

(defun pdf-refresh ()
  "Read the file again — after LaTeX rebuilt it, say — and re-fit the page."
  (let* ((file (buffer-file-name))
         (doc (and file (gethash file *pdf-documents*))))
    (when doc
      (%pdf-load doc file)
      (when (getf doc :count) (%pdf-show doc))))
  nil)

;;; ---------------------------------------------------------------------------
;;; Text

(defun %pdf-pages (doc)
  "DOC's text as a vector of one string per page, extracted on first use."
  (or (getf doc :text)
      (let ((txt (format nil "~apdf-text.txt" (%pdf-cache-dir))))
        (run-process *pdf-program*
                     (list "draw" "-q" "-F" "txt" "-o" txt (buffer-file-name)))
        (handler-case
            (with-open-file (in txt :external-format :utf-8)
              (let* ((s (make-string (file-length in)))
                     (s (subseq s 0 (read-sequence s in))))
                ;; mutool ends every page with a form feed.
                (let ((pages (split-string s #\Page)))
                  (setf (getf doc :text)
                        (coerce (subseq pages 0 (min (getf doc :count) (length pages)))
                                'vector)))))
          (error (e) (message (format nil "pdf: no text — ~a" e)) nil)))))

(defun %pdf-find (direction)
  "Go to the next page in DIRECTION (1 or -1) mentioning `*pdf-last-search*'."
  (let* ((doc (%pdf-doc))
         (pages (and doc *pdf-last-search* (%pdf-pages doc))))
    (when pages
      (let ((count (length pages))
            (from (getf doc :page)))
        (loop for k from 1 to count
              for p = (1+ (mod (+ (1- from) (* direction k)) count))
              when (search *pdf-last-search* (aref pages (1- p)) :test #'char-equal)
                do (%pdf-goto p)
                   (message (format nil "~s on page ~d of ~d~:[~; (wrapped)~]"
                                    *pdf-last-search* p count
                                    (if (plusp direction) (<= p from) (>= p from))))
                   (return)
              finally (message (format nil "~s is not in this document"
                                       *pdf-last-search*))))))
  nil)

(defun pdf-search ()
  "Ask for some text and go to the next page that has it. Case is ignored."
  (when (%pdf-doc)
    (read-string "Search PDF: "
      (lambda (answer)
        (when (and answer (plusp (length answer)))
          (setf *pdf-last-search* answer)
          (%pdf-find 1)))))
  nil)

(defun pdf-search-next ()
  "The next page with what `/' last looked for."
  (%pdf-find 1))

(defun pdf-search-previous ()
  "The previous page with what `/' last looked for."
  (%pdf-find -1))

(defun pdf-text ()
  "The document as plain text in a buffer of its own — to copy out of, or to
search with the usual keys."
  (let* ((doc (%pdf-doc))
         (pages (and doc (%pdf-pages doc)))
         (name (format nil "~a.txt" (buffer-name))))
    (when pages
      (create-buffer name)
      (replace-region 0 (point-max)
                      (with-output-to-string (out)
                        (loop for text across pages for i from 1
                              do (format out "~&--- page ~d ---~%~a" i text))))
      (goto-char 0)))
  nil)

;;; ---------------------------------------------------------------------------
;;; Keys — evil-collection's pdf-view, where it has an opinion.

(loop for (keys command) on
      '("j" "pdf-scroll-down"          "k" "pdf-scroll-up"
        "<down>" "pdf-scroll-down"     "<up>" "pdf-scroll-up"
        "C-d" "pdf-scroll-down-half"   "C-u" "pdf-scroll-up-half"
        "C-f" "pdf-scroll-down-window" "C-b" "pdf-scroll-up-window"
        "<pagedown>" "pdf-scroll-down-window" "<pageup>" "pdf-scroll-up-window"
        "J" "pdf-next-page"            "K" "pdf-previous-page"
        "<right>" "pdf-next-page"      "<left>" "pdf-previous-page"
        "g g" "pdf-first-page"         "G" "pdf-last-page"
        "g t" "pdf-goto-page"          "g r" "pdf-refresh"
        "+" "pdf-zoom-in"              "-" "pdf-zoom-out"
        "=" "pdf-fit-width"            "H" "pdf-fit-page"
        "m" "pdf-midnight"             "t" "pdf-text"
        "/" "pdf-search"               "n" "pdf-search-next"
        "N" "pdf-search-previous"      "q" "kill-buffer")
      by #'cddr
      do (define-mode-key 'pdf-mode keys command))
