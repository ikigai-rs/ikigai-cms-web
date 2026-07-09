# ikigai-cms-web

The **semantic-CMS reading room**: your personal content — bookmarks, notes,
library metadata — as one RDF graph where everything is a tagged, linkable,
queryable resource, served by a kernel and browsed as a query-driven reading
room (a *view is a query*).

Targeted, like `ikigai-web-demo`, at the needs of the CMS: **QUIC/WebTransport-bound,
passkey-authenticated, cap-scoped**. It reuses web-demo's proven plumbing (the
WebTransport kernel server, the WASM wire client, the passkey ceremony) and adds a
CMS server, the reading-room UI, and a server-verified relying party.

## Rungs

1. **The kernel spine** (`build_cms_kernel`) — *this crate, today.* Composes the CMS
   source files (jailed, read through the kernel), the assembled bookmark graph
   (`urn:cms:graph`, org → Turtle via the `ikigai-cms` transreptor), and SPARQL over
   it (`urn:sparql:*`). A `SELECT` over `graph=urn:cms:graph` returns the tagged
   bookmarks — proven by the integration tests.
2. WebTransport server + htmx reading room (XSLT type-renderers over RDF/XML).
3. Server-verified passkey (relying party) → cap-scoped views; cap-on-entry.
4. WebGPU view (a `<cms-graph>` web component; view = query, SHACL-shape renderers).

## The render pipeline (target)

```
view (SPARQL) → CONSTRUCT (align/shape meaning) → RDF/XML → XSLT (type→card) → htmx
```

CONSTRUCT shapes meaning; XSLT shapes pixels; htmx delivers hypermedia; web
components are the interactive islands (the WebGPU graph).

## Status

Rung 1 only. Native kernel library; the WebTransport server and browser front-end
land in the next rungs.
