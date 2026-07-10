<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
  xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
  xmlns:sr="http://www.w3.org/2005/sparql-results#">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <!-- The tag index over SPARQL-results XML: one clickable chip per ?tag / ?n row. No
       attribute value templates and no string functions (xrust lacks them); the hx-get
       is built with xsl:attribute, and the CSS carries no quotes (xrust would escape
       them and corrupt the rule). -->
  <xsl:template match="/sr:sparql">
    <style>
      .cms-tags{display:flex;flex-wrap:wrap;gap:8px;padding:4px 0;align-content:flex-start}
      .cms-tag{display:inline-flex;align-items:baseline;gap:6px;padding:6px 12px;border:1px solid var(--cms-border,rgba(128,128,128,.35));border-radius:999px;text-decoration:none;color:inherit;cursor:pointer;font-size:.95rem}
      .cms-tag:hover{background:var(--cms-accent-soft,rgba(128,128,128,.14))}
      .cms-tag-n{font-size:.78em;opacity:.6}
    </style>
    <section class="cms-tags">
      <xsl:apply-templates select="sr:results/sr:result"/>
    </section>
  </xsl:template>

  <xsl:template match="sr:result">
    <a class="cms-tag" hx-target="#room" hx-swap="innerHTML">
      <xsl:attribute name="hx-get">
        <xsl:text>urn:cms:view:</xsl:text>
        <xsl:value-of select="sr:binding[@name='tag']/sr:literal"/>
      </xsl:attribute>
      <span class="cms-tag-label">
        <xsl:text>#</xsl:text>
        <xsl:value-of select="sr:binding[@name='tag']/sr:literal"/>
      </span>
      <span class="cms-tag-n">
        <xsl:value-of select="sr:binding[@name='n']/sr:literal"/>
      </span>
    </a>
  </xsl:template>
</xsl:stylesheet>
