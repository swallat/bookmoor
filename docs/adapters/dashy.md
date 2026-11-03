# Adapter requirements analysis: Dashy

Status: **open**, nothing verified. Questions to answer:

1. Where does Dashy save edits (is `conf.yml` rewritten on disk, or is the
   edit only held in the browser)?
2. Everything lives in one `conf.yml`. Can the adapter change only its own
   section and leave the rest byte-identical?
3. Which identity strategies survive Dashy's edit UI (see `TEMPLATE.md`)?
4. Section/item structure and nesting limits.
