//! 마이그레이션 파일 찾기와 이름 해석.
//!
//! 원본 설정(`application.yml`): `spring.flyway.validate-migration-naming: true`.
//! 따라서 `.sql` 파일인데 Flyway 이름 규칙(`V<버전>__<설명>.sql`)에 맞지 않으면 실패한다.
//! 이 프로젝트는 버전형(V) 마이그레이션만 쓰므로 반복(R)·실행취소(U)는 지원하지 않는다.

use std::cmp::Ordering;
use std::fmt;
use std::path::Path;

use super::checksum::flyway_checksum;
use super::error::MigrateError;

/// Flyway `MigrationVersion`. 숫자 조각의 나열이며 짧은 쪽은 0으로 채워 비교한다.
#[derive(Clone, Debug)]
pub struct Version {
    parts: Vec<u128>,
}

impl Version {
    /// `202608191752`, `1.2`, `1_2`(= `1.2`)를 받는다. 앞의 0은 무시한다.
    pub fn parse(text: &str) -> Option<Version> {
        if text.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        for piece in text.split(['.', '_']) {
            if piece.is_empty() || !piece.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            parts.push(piece.parse::<u128>().ok()?);
        }
        Some(Version { parts })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.parts.len().max(other.parts.len());
        for i in 0..len {
            let a = self.parts.get(i).copied().unwrap_or(0);
            let b = other.parts.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => {}
                non_eq => return non_eq,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

/// `flyway_schema_history.version`에 들어가는 문자열 형태(`.`로 이은 숫자)다.
impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, part) in self.parts.iter().enumerate() {
            if i > 0 {
                f.write_str(".")?;
            }
            write!(f, "{part}")?;
        }
        Ok(())
    }
}

/// 적용 후보 SQL 마이그레이션 하나.
#[derive(Clone, Debug)]
pub struct Migration {
    pub version: Version,
    /// 파일명의 `_`를 공백으로 바꾼 설명(`flyway_schema_history.description`).
    pub description: String,
    /// 이력의 `script` 열. 위치(디렉터리) 기준 상대 경로다.
    pub script: String,
    pub checksum: i32,
    pub sql: String,
}

impl Migration {
    /// 파일명과 내용으로 마이그레이션을 만든다. 내용은 `include_bytes!`로 내장해도 된다.
    pub fn from_source(script: &str, content: &[u8]) -> Result<Migration, MigrateError> {
        let file_name = script.rsplit('/').next().unwrap_or(script);
        let (version, description) =
            parse_file_name(file_name).map_err(|reason| MigrateError::InvalidName {
                script: script.to_string(),
                reason,
            })?;
        Ok(Migration {
            version,
            description,
            script: script.to_string(),
            checksum: flyway_checksum(content),
            sql: String::from_utf8_lossy(content).into_owned(),
        })
    }
}

fn parse_file_name(file_name: &str) -> Result<(Version, String), String> {
    let stem = file_name
        .strip_suffix(".sql")
        .ok_or_else(|| "접미사는 .sql이어야 한다".to_string())?;
    if stem.starts_with("R__") || stem.starts_with("U") && stem.contains("__") {
        return Err("반복(R)·실행취소(U) 마이그레이션은 지원하지 않는다".to_string());
    }
    let body = stem
        .strip_prefix('V')
        .ok_or_else(|| "접두사는 V여야 한다".to_string())?;
    let (version_text, description) = body
        .split_once("__")
        .ok_or_else(|| "버전과 설명은 __ 로 구분해야 한다".to_string())?;
    let version = Version::parse(version_text)
        .ok_or_else(|| format!("버전 `{version_text}`은 숫자와 . _ 로만 이뤄져야 한다"))?;
    if description.is_empty() {
        return Err("설명이 비어 있다".to_string());
    }
    Ok((version, description.replace('_', " ")))
}

/// 버전 순으로 정렬하고 중복 버전을 거른다.
pub fn sort_and_check(mut migrations: Vec<Migration>) -> Result<Vec<Migration>, MigrateError> {
    migrations.sort_by(|a, b| a.version.cmp(&b.version));
    for pair in migrations.windows(2) {
        if pair[0].version == pair[1].version {
            return Err(MigrateError::DuplicateVersion {
                version: pair[0].version.to_string(),
                first: pair[0].script.clone(),
                second: pair[1].script.clone(),
            });
        }
    }
    Ok(migrations)
}

/// 디렉터리(하위 포함)의 `.sql` 파일을 모두 읽어 버전 순으로 돌려준다.
/// `.sql`이 아닌 파일은 Flyway처럼 무시한다.
pub fn load_dir(dir: &Path) -> Result<Vec<Migration>, MigrateError> {
    let mut found = Vec::new();
    walk(dir, dir, &mut found)?;
    sort_and_check(found)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<Migration>) -> Result<(), MigrateError> {
    let io = |path: &Path, source| MigrateError::Io {
        path: path.display().to_string(),
        source,
    };
    for entry in std::fs::read_dir(dir).map_err(|e| io(dir, e))? {
        let path = entry.map_err(|e| io(dir, e))?.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "sql") {
            let script = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let content = std::fs::read(&path).map_err(|e| io(&path, e))?;
            out.push(Migration::from_source(&script, &content)?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_err(script: &str) -> bool {
        matches!(
            Migration::from_source(script, b"select 1;"),
            Err(MigrateError::InvalidName { .. })
        )
    }

    /// 근거: 원본 파일명 `V202608191752__create_initial_schema.sql`이 이력에는
    /// version `202608191752`, description `create initial schema`로 저장돼 있다.
    #[test]
    fn parses_flyway_name_like_java_history() {
        let m = Migration::from_source("V202608191752__create_initial_schema.sql", b"select 1;")
            .unwrap();
        assert_eq!(m.version.to_string(), "202608191752");
        assert_eq!(m.description, "create initial schema");
        assert_eq!(m.script, "V202608191752__create_initial_schema.sql");
    }

    /// 근거: `validate-migration-naming: true` - 규칙에 맞지 않는 .sql 이름은 실패한다.
    #[test]
    fn rejects_names_that_break_flyway_naming_rule() {
        assert!(name_err("v1__lowercase.sql"));
        assert!(name_err("1__no_prefix.sql"));
        assert!(name_err("V1_single_underscore.sql"));
        assert!(name_err("V__no_version.sql"));
        assert!(name_err("Vabc__letters.sql"));
        assert!(name_err("V1__.sql"));
        assert!(name_err("R__repeatable.sql"));
        assert!(name_err("U1__undo.sql"));
        assert!(name_err("V1.2.__trailing_dot.sql"));
    }

    #[test]
    fn version_ordering_is_numeric_and_zero_padded() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("9") < v("10"));
        assert!(v("1.2") < v("1.10"));
        assert_eq!(v("1"), v("1.0"));
        assert_eq!(v("001"), v("1"));
        assert_eq!(v("1_2").to_string(), "1.2");
        assert!(v("202608191752") < v("202608202034"));
    }

    #[test]
    fn duplicate_versions_are_rejected_even_when_written_differently() {
        let a = Migration::from_source("V1__a.sql", b"a").unwrap();
        let b = Migration::from_source("V1.0__b.sql", b"b").unwrap();
        assert!(matches!(
            sort_and_check(vec![a, b]),
            Err(MigrateError::DuplicateVersion { .. })
        ));
    }

    /// 근거: 저장소 `migrations/`는 45개이며 Java가 적용한 순서(버전 오름차순)와 같아야 한다.
    #[test]
    fn loads_repository_migrations_in_version_order() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../migrations");
        let all = load_dir(&dir).unwrap();
        assert_eq!(all.len(), 45);
        assert!(all.windows(2).all(|w| w[0].version < w[1].version));
        assert_eq!(all[0].script, "V202608191752__create_initial_schema.sql");
        assert_eq!(all[44].description, "allow notice delete audit");
    }

    #[test]
    fn non_sql_files_are_ignored_and_bad_sql_names_fail() {
        let dir = std::env::temp_dir().join(format!("chalkak-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        std::fs::write(dir.join("V2__b.sql"), "select 2;").unwrap();
        std::fs::write(dir.join("V1__a.sql"), "select 1;").unwrap();
        let ok = load_dir(&dir).unwrap();
        assert_eq!(
            ok.iter().map(|m| m.script.as_str()).collect::<Vec<_>>(),
            ["V1__a.sql", "V2__b.sql"]
        );
        std::fs::write(dir.join("bad.sql"), "select 3;").unwrap();
        assert!(matches!(
            load_dir(&dir),
            Err(MigrateError::InvalidName { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
