unit PrivateProperty;

interface

type
  TFoo = class
  private
    FStorage: Integer;
    property InternalValue: Integer read FStorage;
  end;

implementation

end.
