<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
  xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
  xmlns:r="urn:cms:recent#">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <!-- The recency trail over a small session-supplied doc (urn:cms:recent#): one row per
       viewed resource, most-recent first. Authored to xrust's subset — no conditionals
       (the empty state is its own element), xsl:attribute for hx-get (no AVT), and no
       quotes in the CSS (xrust would escape them and corrupt the rule). -->
  <xsl:template match="/r:recent">
    <style>
      .cms-recent{display:flex;flex-direction:column;gap:6px;padding:4px 0}
      .cms-recent-item{padding:8px 12px;border:1px solid var(--cms-border,rgba(128,128,128,.35));border-radius:10px;text-decoration:none;color:inherit;cursor:pointer}
      .cms-recent-item:hover{background:var(--cms-accent-soft,rgba(128,128,128,.14))}
      .cms-recent-empty{color:var(--cms-mut,rgba(128,128,128,.85));font-size:.95rem;padding:4px 0}
    </style>
    <section class="cms-recent">
      <xsl:apply-templates/>
    </section>
  </xsl:template>

  <xsl:template match="r:item">
    <a class="cms-recent-item" hx-target="#room" hx-swap="innerHTML">
      <!-- `/r/{iri}?type={scope}` — the recorded type scope reopens the tag within its kind
           (empty scope = unscoped; the server treats `type=` as no filter). -->
      <xsl:attribute name="hx-get">
        <xsl:text>/r/</xsl:text>
        <xsl:value-of select="@iri"/>
        <xsl:text>?type=</xsl:text>
        <xsl:value-of select="@scope"/>
      </xsl:attribute>
      <xsl:value-of select="."/>
    </a>
  </xsl:template>

  <xsl:template match="r:empty">
    <p class="cms-recent-empty">Nothing viewed yet — open a tag and it shows up here.</p>
  </xsl:template>
</xsl:stylesheet>
