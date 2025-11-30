use poker_engine::api::dto::TournamentViewDto;
use poker_engine::domain::tournament::{Tournament, TournamentStatus};

/// Построить DTO турнира из доменной модели.
pub fn build_tournament_view(
    t: &Tournament,
    tables_running: u32,
) -> TournamentViewDto {
    // Преобразуем статус в строку для UI
    let status_string = match t.status {
        TournamentStatus::Registering => "Registering",
        TournamentStatus::Running => "Running",
        TournamentStatus::OnBreak => "OnBreak",
        TournamentStatus::Finished => "Finished",
    }
    .to_string();

    TournamentViewDto {
        tournament_id: t.id,                     // <-- ИСПРАВЛЕНО
        name: t.config.name.clone(),             // <-- ИСПРАВЛЕНО
        status: status_string,
        current_level: t.current_level,          // <-- корректно
        players_registered: t.registrations.len() as u32,
        tables_running,
    }
}
