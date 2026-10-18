from app.youtube import safe_filename, split_title, video_id_from


def test_artist_and_title_from_video_title():
    assert split_title("Daft Punk - Get Lucky (Official Audio) ft. Pharrell Williams, Nile Rodgers") == (
        "Daft Punk", "Get Lucky ft. Pharrell Williams, Nile Rodgers"
    )
    assert split_title("Daft Punk : Get Lucky // Grammys Rehearsal (HD)") == (
        "Daft Punk", "Get Lucky // Grammys Rehearsal"
    )
    assert split_title('Queen – "Bohemian Rhapsody" [Official Video Remastered]') == (
        "Queen", "Bohemian Rhapsody"
    )


def test_channel_is_artist_when_title_has_no_separator():
    assert split_title("Bad Guy (Official Music Video)", "BillieEilishVEVO") == ("BillieEilish", "Bad Guy")
    assert split_title("Midnight City", "M83 - Topic") == ("M83", "Midnight City")


def test_youtube_music_metadata_wins():
    assert split_title("whatever", "x", artist="ABBA, Someone", track="Dancing Queen") == (
        "ABBA", "Dancing Queen"
    )


def test_video_id_from_urls():
    for text in (
        "dQw4w9WgXcQ",
        "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
        "https://youtube.com/watch?list=PL1&v=dQw4w9WgXcQ&t=3",
        "https://youtu.be/dQw4w9WgXcQ?si=abc",
        "https://www.youtube.com/shorts/dQw4w9WgXcQ",
        "https://music.youtube.com/watch?v=dQw4w9WgXcQ",
    ):
        assert video_id_from(text) == "dQw4w9WgXcQ", text
    assert video_id_from("never gonna give you up") is None


def test_safe_filename():
    assert safe_filename('AC/DC - Back In Black: "Live"?') == "ACDC - Back In Black Live"
