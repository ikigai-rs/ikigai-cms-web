<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
  xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
  xmlns:rev="urn:cms:review#">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <!-- The suggested-deletes review over a server-supplied doc (urn:cms:review#). One `section` per
       removal category (gone / durably-unreachable), each carrying its own `@label`, purge `@action`
       IRI, and `@cta` button text; each section renders a header (with its purge button) and a grid
       of item cards. A trailing `note` reports the count still under observation. Authored to
       xrust's subset — no conditionals (each section/note is just a template match; empty is its own
       element), xsl:attribute for the dynamic hx-get/href/class (no AVT), class-selector CSS only
       (no attribute-selector quotes). Status colour is a per-status class. -->
  <xsl:template match="/rev:review">
    <style>
      .cms-review-head{color:var(--mut,#888);font-size:13px;margin:14px 0 10px;display:flex;align-items:center;gap:8px;flex-wrap:wrap}
      .cms-review{display:grid;grid-template-columns:repeat(auto-fit,minmax(280px,1fr));gap:12px;margin-bottom:6px}
      .cms-dead-card{border:1px solid var(--line,rgba(128,128,128,.35));border-left-width:3px;border-radius:12px;padding:12px 14px}
      .cms-dead-card.gone{border-left-color:#c0392b}
      .cms-dead-card.unreachable{border-left-color:#c07a1f}
      .cms-dead-badge{display:inline-block;font-size:10.5px;letter-spacing:.08em;text-transform:uppercase;font-weight:700;margin-bottom:3px}
      .cms-dead-badge.gone{color:#c0392b}
      .cms-dead-badge.unreachable{color:#c07a1f}
      .cms-dead-title{display:block;color:inherit;text-decoration:none;font-size:14px;font-weight:500;line-height:1.4}
      .cms-dead-title:hover{text-decoration:underline}
      .cms-dead-url{display:block;color:var(--mut,#888);font-size:11px;word-break:break-all;margin:2px 0}
      .cms-dead-meta{color:var(--mut,#888);font-size:12px}
      .cms-review-empty{color:var(--mut,#888);font-size:.95rem;padding:6px 0}
      .cms-review-note{color:var(--mut,#888);font-size:12px;padding:8px 0;font-style:italic}
      .cms-review-purge{font:13px system-ui;padding:4px 12px;border:1px solid #c0392b;border-radius:8px;background:transparent;color:#c0392b;cursor:pointer}
      .cms-review-purge:hover{background:#c0392b;color:#fff}
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
        <xsl:value-of select="@reason"/><xsl:text> · dead </xsl:text>
        <xsl:value-of select="@days"/><xsl:text>d</xsl:text>
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
