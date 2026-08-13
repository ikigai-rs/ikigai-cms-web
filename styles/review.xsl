<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
  xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
  xmlns:rev="urn:cms:review#">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <!-- The suggested-deletes review over a server-supplied doc (urn:cms:review#). One `section` per
       REMOVAL set (dead domains / gone / durably-unreachable), each carrying its own `@label`,
       purge `@action` IRI, and `@cta` button text: header with its purge button, then a grid of
       item cards. A `flagged` block is the same grid with NO bulk action — for classes that are
       unverified rather than dead; the missing button is structural because xrust has no
       conditionals. A trailing `note` reports the count still under observation.

       Every card carries per-card `remove` / `keep` buttons posting to `@enc` (the URL,
       percent-encoded server-side — xrust has no string functions, so it could not escape a URL
       into JSON for hx-vals). The card's duration comes pre-phrased in `@age`: whether "9d" means
       "dead" or "unverified" is an evidence question, and the stylesheet is not where it gets
       decided. Authored to xrust's subset — xsl:attribute for the dynamic hx-post/href/class (no
       AVT), class-selector CSS only (no attribute-selector quotes). -->
  <xsl:template match="/rev:review">
    <style>
      .cms-review-head{color:var(--mut,#888);font-size:13px;margin:14px 0 10px;display:flex;align-items:center;gap:8px;flex-wrap:wrap}
      .cms-review{display:grid;grid-template-columns:repeat(auto-fit,minmax(280px,1fr));gap:12px;margin-bottom:6px}
      .cms-dead-card{border:1px solid var(--line,rgba(128,128,128,.35));border-left-width:3px;border-radius:12px;padding:12px 14px}
      .cms-dead-card.gone{border-left-color:#c0392b}
      .cms-dead-card.nxdomain{border-left-color:#8e2f8e}
      .cms-dead-card.unreachable{border-left-color:#c07a1f}
      .cms-dead-card.refused{border-left-color:#2b7bb9}
      .cms-dead-badge{display:inline-block;font-size:10.5px;letter-spacing:.08em;text-transform:uppercase;font-weight:700;margin-bottom:3px}
      .cms-dead-badge.gone{color:#c0392b}
      .cms-dead-badge.nxdomain{color:#8e2f8e}
      .cms-dead-badge.unreachable{color:#c07a1f}
      .cms-dead-badge.refused{color:#2b7bb9}
      .cms-dead-title{display:block;color:inherit;text-decoration:none;font-size:14px;font-weight:500;line-height:1.4}
      .cms-dead-title:hover{text-decoration:underline}
      .cms-dead-url{display:block;color:var(--mut,#888);font-size:11px;word-break:break-all;margin:2px 0}
      .cms-dead-meta{color:var(--mut,#888);font-size:12px}
      .cms-review-empty{color:var(--mut,#888);font-size:.95rem;padding:6px 0}
      .cms-review-note{color:var(--mut,#888);font-size:12px;padding:8px 0;font-style:italic}
      .cms-review-purge{font:13px system-ui;padding:4px 12px;border:1px solid #c0392b;border-radius:8px;background:transparent;color:#c0392b;cursor:pointer}
      .cms-review-purge:hover{background:#c0392b;color:#fff}
      .cms-review-flagged{color:var(--mut,#888);font-size:13px;margin:14px 0 10px;max-width:70ch;line-height:1.5}
      .cms-card-acts{display:flex;gap:6px;margin-top:8px}
      .cms-card-act{font:11.5px system-ui;padding:2px 10px;border:1px solid var(--line,rgba(128,128,128,.35));border-radius:7px;background:transparent;color:var(--mut,#888);cursor:pointer}
      .cms-card-act.remove:hover{border-color:#c0392b;color:#c0392b}
      .cms-card-act.keep:hover{border-color:#2e8b57;color:#2e8b57}
      .cms-card-done{display:inline-block;font-size:11.5px;color:var(--mut,#888);padding:8px 2px;font-style:italic}
    </style>
    <xsl:apply-templates/>
  </xsl:template>

  <xsl:template match="rev:section">
    <div class="cms-review-head">
      <xsl:value-of select="@label"/>
      <button class="cms-review-purge">
        <xsl:attribute name="hx-get">/r/<xsl:value-of select="@action"/></xsl:attribute>
        <xsl:value-of select="@cta"/>
      </button>
    </div>
    <section class="cms-review">
      <xsl:apply-templates select="rev:item"/>
    </section>
  </xsl:template>

  <xsl:template match="rev:flagged">
    <p class="cms-review-flagged"><xsl:value-of select="@label"/></p>
    <section class="cms-review">
      <xsl:apply-templates select="rev:item"/>
    </section>
  </xsl:template>

  <xsl:template match="rev:item">
    <article>
      <xsl:attribute name="class">cms-dead-card <xsl:value-of select="@status"/></xsl:attribute>
      <span>
        <xsl:attribute name="class">cms-dead-badge <xsl:value-of select="@status"/></xsl:attribute>
        <xsl:value-of select="@status"/>
      </span>
      <a class="cms-dead-title" target="_blank" rel="noopener noreferrer">
        <xsl:attribute name="href"><xsl:value-of select="@url"/></xsl:attribute>
        <xsl:value-of select="@title"/>
      </a>
      <span class="cms-dead-url"><xsl:value-of select="@url"/></span>
      <div class="cms-dead-meta">
        <xsl:value-of select="@reason"/><xsl:text> · </xsl:text>
        <xsl:value-of select="@age"/>
      </div>
      <div class="cms-card-acts">
        <button class="cms-card-act remove" hx-target="closest article" hx-swap="outerHTML">
          <xsl:attribute name="hx-post">/link/remove?url=<xsl:value-of select="@enc"/></xsl:attribute>
          <xsl:text>remove</xsl:text>
        </button>
        <button class="cms-card-act keep" hx-target="closest article" hx-swap="outerHTML">
          <xsl:attribute name="hx-post">/link/keep?url=<xsl:value-of select="@enc"/></xsl:attribute>
          <xsl:text>keep</xsl:text>
        </button>
      </div>
    </article>
  </xsl:template>

  <xsl:template match="rev:note">
    <p class="cms-review-note"><xsl:value-of select="."/></p>
  </xsl:template>

  <xsl:template match="rev:empty">
    <p class="cms-review-empty">No removal candidates — nothing to clean up yet.</p>
  </xsl:template>
</xsl:stylesheet>
