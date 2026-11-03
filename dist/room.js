// Focus and the view's name, after an htmx swap (ledger #1641, SC 2.4.3, 4.1.3).
//
// A control inside the room swaps itself away when it works: a pager button, a chip, a purge
// button. Focus then falls to the body, and a keyboard or screen-reader user starts again from the
// top of the page. So: when a swap removes the element that had focus, and only then, move focus to
// where the person now is. The view's heading after a whole view; the new content after a swap of
// one part. A swap that leaves focus alone (the navigation bar, the search field, the link-check
// poll) keeps it, and the status line names the new view instead.
//
// It also keeps the page title on the current view. Nothing here is needed to use the room with a
// mouse, and the room works without it.
(function () {
  const BASE_TITLE = "ikigai — reading room";
  const FOCUSABLE = "a[href], button:not([disabled]), input:not([type=hidden])";
  const room = document.getElementById("room");
  const status = document.getElementById("room-status");
  // Each request in flight, by its swap target. `target` itself may be gone after the swap, so
  // where it sits is noted now.
  const pending = new Map();

  function say(text) {
    status.textContent = text;
  }

  function focusOn(el) {
    if (!el.matches(FOCUSABLE)) el.tabIndex = -1;
    el.focus();
  }

  // beforeRequest, not beforeSwap: the wire's path (wire.js) cancels the request here and calls
  // htmx.swap itself, which fires no beforeSwap and no afterRequest.
  document.body.addEventListener("htmx:beforeRequest", (evt) => {
    const target = evt.detail.target;
    if (!target) return;
    const held = document.activeElement;
    pending.set(target, {
      held: held && held !== document.body ? held : null,
      parent: target.parentElement,
      previous: target.previousElementSibling,
      next: target.nextElementSibling,
    });
  });

  function viewSwapped(lostFocus) {
    const heading = room.querySelector("h2");
    const name = heading ? heading.textContent.trim() : "";
    document.title = name ? name + " — ikigai reading room" : BASE_TITLE;
    if (lostFocus) {
      focusOn(heading || room);
    } else if (name) {
      // The pager's position, so paging within one view is not silent.
      const page = room.querySelector(".cms-page-info");
      say(page ? name + ", " + page.textContent : name);
    }
  }

  function partSwapped({ parent, previous, next }, lostFocus) {
    if (!parent.isConnected) return;
    const nextLeft = next && next.isConnected ? next : null;
    const placed =
      previous && previous.isConnected ? previous.nextElementSibling : parent.firstElementChild;
    if (placed && placed !== nextLeft) {
      // The answer took its place: a promoted tag's chip, "kept", "removed", or a failure.
      if (lostFocus) focusOn(placed);
      return;
    }
    // Nothing took its place (a dismissed suggestion): the nearest control that is left.
    say("dismissed");
    if (lostFocus) {
      const near =
        nextLeft && (nextLeft.matches(FOCUSABLE) ? nextLeft : nextLeft.querySelector(FOCUSABLE));
      focusOn(near || parent.querySelector(FOCUSABLE) || room.querySelector("h2") || room);
    }
  }

  function swapped(target) {
    const seen = pending.get(target);
    if (!seen) return;
    pending.delete(target);
    const active = document.activeElement;
    const lostFocus =
      !!seen.held && !seen.held.isConnected && (!active || active === document.body);
    if (target === room) {
      viewSwapped(lostFocus);
    } else if (seen.parent && room.contains(seen.parent) && !target.isConnected) {
      partSwapped(seen, lostFocus);
    }
  }

  // afterRequest, not afterSwap: that fires on the new elements, and an empty answer has none.
  // htmx swaps before it, and sends it to the nearest ancestor left if the element has gone.
  document.body.addEventListener("htmx:afterRequest", (evt) => {
    if (evt.detail.successful) swapped(evt.detail.target);
    else pending.delete(evt.detail.target);
  });
  // The same moment on the wire's path.
  document.body.addEventListener("cms:swapped", (evt) => swapped(evt.detail.target));
})();
