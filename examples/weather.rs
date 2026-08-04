//! Ask the National Weather Service for real, and print what the model would see.
//!
//! The unit tests are folds over recorded bodies, which proves the parsing and
//! not that the endpoints still answer the way they did. This is the other
//! half — the same seam `examples/ask.rs` is for the transport.
//!
//! ```sh
//! cargo run --example weather                    # the configured location
//! cargo run --example weather -- 40.0529 -83.0925
//! ```

use familiar::model::settings::Config;
use familiar::model::turn::ToolOutcome;
use familiar::model::weather::Point;
use familiar::ui::runner::Runner;
use gtk::glib;

fn main() {
    let mut arguments = std::env::args().skip(1);
    let at = match (arguments.next(), arguments.next()) {
        (Some(latitude), Some(longitude)) => Some(Point {
            latitude: latitude.parse().expect("a latitude"),
            longitude: longitude.parse().expect("a longitude"),
        }),
        _ => {
            let (config, _) = Config::load(&Config::default_path());
            config.weather_point()
        }
    };
    match at {
        Some(point) => println!("Asking about {}…\n", point.as_query()),
        None => println!("No location configured; expect the tool to say so.\n"),
    }

    let runner =
        Runner::new(std::rc::Rc::new(std::cell::RefCell::new(None)), None).with_weather(at);

    let call = familiar::model::turn::ToolCall {
        id: "call_1".into(),
        name: "weather".into(),
        arguments: "{}".into(),
        complete: true,
        outcome: None,
    };

    let main_loop = glib::MainLoop::new(None, false);
    runner.run(&call, {
        let main_loop = main_loop.clone();
        move |outcome| {
            match outcome {
                ToolOutcome::Ok(said) => println!("{said}"),
                other => println!("{other:?}"),
            }
            main_loop.quit();
        }
    });
    main_loop.run();
}
