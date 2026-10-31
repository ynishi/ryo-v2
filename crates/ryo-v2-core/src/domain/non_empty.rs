//! A list that cannot be empty.

/// A `Vec<T>` with at least one element. The only way in is [`NonEmpty::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    /// `None` for an empty vector.
    ///
    /// # Contract
    /// - C1: returns `None` if and only if `items` is empty.
    ///   tested: domain::non_empty::tests::empty_is_refused
    pub fn new(items: Vec<T>) -> Option<Self> {
        if items.is_empty() {
            None
        } else {
            Some(Self(items))
        }
    }

    /// A list of one element.
    pub fn one(item: T) -> Self {
        Self(vec![item])
    }

    /// The elements, in order.
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// The first element, which always exists.
    pub fn first(&self) -> &T {
        &self.0[0]
    }

    /// The last element, which always exists.
    pub fn last(&self) -> &T {
        &self.0[self.0.len() - 1]
    }

    /// How many elements there are (at least 1).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Iterates the elements in order.
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.0.iter()
    }

    /// Gives the elements back as a plain vector.
    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_refused() {
        assert!(NonEmpty::<u8>::new(vec![]).is_none());
        let n = NonEmpty::new(vec![3, 4]).unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(*n.first(), 3);
        assert!(!n.is_empty());
        assert_eq!(n.iter().count(), 2);
        assert_eq!(n.as_slice(), &[3, 4]);
        assert_eq!(*n.last(), 4);
        assert_eq!(n.into_vec(), vec![3, 4]);
        let one = NonEmpty::one(9);
        assert_eq!((*one.first(), *one.last(), one.len()), (9, 9, 1));
    }
}
