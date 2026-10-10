-- Optional free-text repository description shown in the console. Seeded
-- repositories carry a bilingual description; user-created ones default empty.
ALTER TABLE repositories ADD COLUMN description TEXT NOT NULL DEFAULT '';
