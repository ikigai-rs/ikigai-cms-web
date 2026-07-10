<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <xsl:template match="/rdf:RDF">
    <style>.cms-room{display:flex;flex-direction:column;gap:10px;font-family:Georgia,serif}
.cms-count{color:var(--mut);font-size:13px;margin-bottom:2px}
.cms-card{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:12px 16px}
.cms-title{display:block;color:var(--fg);text-decoration:none;font-size:16px}
.cms-title:hover{text-decoration:underline}
.cms-tags{display:flex;flex-wrap:wrap;gap:6px;margin-top:8px}
.cms-tag{font-size:12px;font-family:ui-monospace,monospace;color:var(--mut);text-decoration:none;cursor:pointer}</style>
    <section class="cms-room">
      <div class="cms-count"><xsl:value-of select="count(rdf:Description)"/> resources</div>
      <xsl:apply-templates select="rdf:Description"/>
    </section>
  </xsl:template>

  <xsl:template match="rdf:Description">
    <article class="cms-card">
      <a class="cms-title" target="_blank" rel="noopener noreferrer">
        <xsl:attribute name="href"><xsl:value-of select="dc:identifier"/></xsl:attribute>
        <xsl:value-of select="dc:title"/>
      </a>
      <div class="cms-tags">
        <xsl:for-each select="dc:subject">
          <a class="cms-tag" hx-target="#room">
            <xsl:attribute name="hx-get">urn:cms:view:<xsl:value-of select="."/></xsl:attribute>
            <xsl:text>#</xsl:text><xsl:value-of select="."/>
          </a>
        </xsl:for-each>
      </div>
    </article>
  </xsl:template>
</xsl:stylesheet>
