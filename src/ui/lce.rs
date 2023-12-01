use std::marker::PhantomData;

pub type DynamicLce<C, E> = Lce<Box<dyn Loadable<C, E>>, C, E>;

impl<C, E> Loadable<C, E> for Box<dyn Loadable<C, E>> {
    fn ready(&mut self) -> bool {
        self.as_mut().ready()
    }

    fn take(&mut self) -> Result<C, E> {
        self.as_mut().take()
    }
}

pub enum Lce<L, C, E> {
    Waiting,
    Loading(L),
    Content(C),
    Error(E),
}

impl<L, C, E> Default for Lce<L, C, E> {
    fn default() -> Self {
        Self::Waiting
    }
}

pub struct MappedLoadable<F, L, C, E, Co, Eo>(L, F, PhantomData<(C, E, Co, Eo)>);

impl<F, L, C, E, Co, Eo> Loadable<Co, Eo> for MappedLoadable<F, L, C, E, Co, Eo>
where
    L: Loadable<C, E>,
    F: FnMut(Result<C, E>) -> Result<Co, Eo>,
{
    fn take(&mut self) -> Result<Co, Eo> {
        (&mut self.1)(self.0.take())
    }

    fn ready(&mut self) -> bool {
        self.0.ready()
    }
}

pub trait Loadable<C, E> {
    fn ready(&mut self) -> bool;
    fn take(&mut self) -> Result<C, E>;
}

pub trait LoadableExt<C, E>: Sized {
    fn map_res<F, Co, Eo>(self, convert: F) -> MappedLoadable<F, Self, C, E, Co, Eo>
    where
        F: FnMut(Result<C, E>) -> Result<Co, Eo>;

    // These should be doable, but are not currently due to instability of RPITIT
    // fn map<F, Co>(
    //     self,
    //     convert: F,
    // ) -> MappedLoadable<impl FnOnce(Result<C, E>) -> Result<Co, E>, Self, C, E, Co, E>
    // where
    //     F: FnOnce(C) -> Co,
    // {
    //     self.map_res(|it: Result<C, E>| it.map(convert))
    // }

    // fn map_err<F, Eo>(self, convert: F) -> MappedLoadable<F, Self, C, E, C, Eo>
    // where
    //     F: FnOnce(E) -> Eo,
    // {
    //     self.map_res(|it: E| it.map_err(convert))
    // }
}

impl<L, C, E> LoadableExt<C, E> for L
where
    L: Loadable<C, E>,
{
    fn map_res<F, Co, Eo>(self, convert: F) -> MappedLoadable<F, Self, C, E, Co, Eo>
    where
        F: FnMut(Result<C, E>) -> Result<Co, Eo>,
    {
        MappedLoadable(self, convert, PhantomData::default())
    }
}

impl<L, C, E> Lce<L, C, E>
where
    L: Loadable<C, E>,
{
    fn process(&mut self) {
        match std::mem::replace(self, Lce::Waiting) {
            Lce::Loading(mut loading) => {
                if loading.ready() {
                    match loading.take() {
                        Ok(content) => {
                            *self = Lce::Content(content);
                        }

                        Err(e) => {
                            *self = Lce::Error(e);
                        }
                    }
                } else {
                    *self = Lce::Loading(loading);
                }
            }

            it => {
                *self = it;
            }
        }
    }

    #[inline]
    pub fn to_dyn_lce(self) -> DynamicLce<C, E>
    where
        L: 'static,
    {
        match self {
            Lce::Waiting => Lce::Waiting,
            Lce::Loading(l) => Lce::Loading(Box::new(l) as Box<dyn Loadable<C, E> + 'static>),
            Lce::Content(c) => Lce::Content(c),
            Lce::Error(e) => Lce::Error(e),
        }
    }

    pub fn ui<R>(
        &mut self,
        ui: &mut egui::Ui,
        on_waiting: impl FnOnce(&mut egui::Ui) -> R,
        on_loading: impl FnOnce(&mut egui::Ui, &mut L) -> R,
        on_content: impl FnOnce(&mut egui::Ui, &mut C) -> R,
        on_error: impl FnOnce(&mut egui::Ui, &mut E) -> R,
    ) -> egui::InnerResponse<R> {
        self.process();

        ui.allocate_ui(ui.available_size(), |ui| match self {
            Lce::Waiting => on_waiting(ui),
            Lce::Loading(l) => on_loading(ui, l),
            Lce::Content(c) => on_content(ui, c),
            Lce::Error(e) => on_error(ui, e),
        })
    }

    pub fn show(&mut self, ui: &mut egui::Ui, on_waiting: impl FnOnce(&mut egui::Ui))
    where
        for<'a> &'a mut L: egui::Widget,
        for<'a> &'a mut C: egui::Widget,
        for<'a> &'a mut E: egui::Widget,
    {
        self.ui(
            ui,
            on_waiting,
            |ui, l| {
                ui.add(l);
            },
            |ui, c| {
                ui.add(c);
            },
            |ui, e| {
                ui.add(e);
            },
        );
    }
}
